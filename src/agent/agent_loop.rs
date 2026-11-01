//! The agent loop. Extension behavior is supplied through bounded, ordered hooks.

use std::collections::HashSet;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::join_all;
use futures::{FutureExt, StreamExt};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::events::{AgentEvent, ApprovalResult, Emit, EventSink, LoopOutcome, RequestKind, RunTrace, ToolVerdict};
use super::hooks::{
    ContextEdit, HookCall, HookChain, HookError, HookFailure, HookPhase, TurnDecision, TurnDisposition, TurnSummary,
    report_failure,
};
use super::host::{ContextPreparation, SessionHost};
use super::policy::Decision;
use super::queue::{MessageQueues, Queue};
use super::tool::{Effects, ExecutionMode, Tool, ToolContext, ToolOutput};
use crate::ai::{
    AssistantMessage, Message, Provider, Request, StopReason, StreamEvent, SystemMessage, SystemPrompt, ToolCall,
    ToolResultMessage, Usage, UserMessageKind, duration_ms, now_ms,
};
use crate::interaction::{Approval, ApprovalCache, ApprovalRequest, DEFAULT_APPROVAL_TIMEOUT, Interaction, NoHuman};

/// Static model and tool configuration, independent of per-run state and extensions.
pub struct LoopSpec {
    pub provider: Arc<dyn Provider>,
    pub model: String,
    /// Changes are recorded as system messages before the next request.
    pub system_prompt: SystemPrompt,
    pub tools: Vec<Arc<dyn Tool>>,
    pub cwd: PathBuf,
    pub max_tokens: Option<u32>,
    pub sequential_tools: bool,
}

impl LoopSpec {
    /// Check what the loop relies on: the model calls tools by name, so no two tools may
    /// share one. `run_loop` checks before its first request.
    pub fn validate(&self) -> Result<(), String> {
        let mut names = HashSet::new();
        for tool in &self.tools {
            if !names.insert(tool.name()) {
                return Err(format!("more than one tool is named {:?}", tool.name()));
            }
        }
        Ok(())
    }
}

/// Host extensions and the static specification. Queues have shared ownership because
/// frontends can submit input while a run borrows this configuration.
pub struct LoopConfig {
    pub spec: LoopSpec,
    pub hooks: HookChain,
    pub interaction: Arc<dyn Interaction>,
    pub interaction_timeout: Duration,
    pub queues: Arc<MessageQueues>,
}

impl LoopConfig {
    pub fn new(spec: LoopSpec) -> Self {
        Self {
            spec,
            hooks: HookChain::new(),
            interaction: Arc::new(NoHuman),
            interaction_timeout: DEFAULT_APPROVAL_TIMEOUT,
            queues: Arc::new(MessageQueues::default()),
        }
    }
}

/// State borrowed from the run owner; grants survive retries without sharing a lock.
pub struct LoopControl<'a> {
    /// Model requests this attempt may make, summary requests included; `None` is unlimited.
    pub max_turns: Option<u32>,
    /// Task turns of earlier attempts, so that turn numbers continue across retries.
    pub turn_offset: u32,
    pub continuations: u32,
    pub approvals: &'a mut ApprovalCache,
    pub cancel: &'a CancellationToken,
    pub sink: &'a dyn EventSink,
    /// Records each step before the loop acts on it; the Runner supplies its session. Without
    /// one, the loop keeps messages and edits only in its context.
    pub(crate) host: Option<&'a mut dyn SessionHost>,
    /// The run this attempt belongs to; without one, `run_loop` is a run of its own.
    pub(crate) trace: Option<&'a RunTrace>,
    /// 1 for the first attempt of a run, plus one for each retry.
    pub(crate) attempt: u32,
}

impl<'a> LoopControl<'a> {
    /// Control for a run that keeps its messages only in the context passed to `run_loop`.
    pub fn new(
        max_turns: Option<u32>,
        approvals: &'a mut ApprovalCache,
        cancel: &'a CancellationToken,
        sink: &'a dyn EventSink,
    ) -> Self {
        Self {
            max_turns,
            turn_offset: 0,
            continuations: 0,
            approvals,
            cancel,
            sink,
            host: None,
            trace: None,
            attempt: 1,
        }
    }
}

/// Accounting follows actual responses, so context edits cannot change usage or retries.
#[derive(Debug)]
pub enum LoopFailure {
    /// A failed or cancelled response. Boxed so a successful LoopResult does not reserve
    /// space for a full response.
    Provider {
        message: Box<AssistantMessage>,
        /// The response's session entry, when the run records to a session.
        entry_id: Option<String>,
    },
    /// A hook or context preparation failed, or the session could not record.
    Host(String),
}

/// Result of one loop attempt. Only provider failures qualify for provider retries.
#[derive(Debug)]
pub struct LoopResult {
    pub outcome: LoopOutcome,
    /// Task turns started, numbered from `turn_offset + 1`.
    pub turns: u32,
    /// Model requests made, summary requests included: what the budget counts.
    pub requests: u32,
    pub continuations: u32,
    pub usage: Usage,
    pub failure: Option<LoopFailure>,
}

/// Append prompts and run until the model stops, the budget expires, or cancellation.
/// Each step is recorded before the loop acts on it; the provider sees exactly this context.
/// None imposes no turn limit, while Some(0) makes no model request.
pub async fn run_loop(
    context: &mut Vec<Message>,
    prompts: Vec<Message>,
    config: &LoopConfig,
    control: &mut LoopControl<'_>,
) -> LoopResult {
    let mut result = LoopResult {
        outcome: LoopOutcome::Completed,
        turns: 0,
        requests: 0,
        continuations: control.continuations,
        usage: Usage::default(),
        failure: None,
    };
    let own_trace;
    let trace = match control.trace {
        Some(trace) => trace,
        None => {
            own_trace = RunTrace::new(None, config.spec.cwd.clone(), control.cancel.clone());
            &own_trace
        }
    };
    let mut engine = Engine {
        config,
        control,
        trace,
        turn: None,
    };
    let attempt = engine.control.attempt;
    engine.emit(AgentEvent::AgentStart { attempt }).await;
    match engine.run(context, prompts, &mut result).await {
        Ok(outcome) => result.outcome = outcome,
        Err(halt) => {
            let (aborted, error) = match halt {
                Halt::Failed(error) => (engine.control.cancel.is_cancelled(), error),
                // Even when the run was also cancelled: its audit trail is incomplete.
                Halt::Unrecorded(error) => (false, error),
            };
            result.outcome = if aborted {
                LoopOutcome::Aborted
            } else {
                LoopOutcome::Error
            };
            result.failure = Some(LoopFailure::Host(error));
        }
    }
    // A turn that stopped before its request finished still ends, so turn events pair up.
    engine.end_turn().await;
    engine
        .emit(AgentEvent::AgentEnd {
            outcome: result.outcome,
        })
        .await;
    result
}

/// Why the loop stopped before the model finished.
enum Halt {
    /// A hook or context preparation failed; if the run was cancelled, it was aborted.
    Failed(String),
    /// The session could not record a step, so the loop must not act on it.
    Unrecorded(String),
}

struct Engine<'a, 'b, 'c> {
    config: &'a LoopConfig,
    control: &'b mut LoopControl<'c>,
    trace: &'b RunTrace,
    /// The number of the turn in progress, between its `turn_start` and `turn_end`.
    turn: Option<u32>,
}

impl<'a> Engine<'a, '_, '_> {
    fn to(&self) -> Emit<'_> {
        Emit {
            sink: self.control.sink,
            trace: self.trace,
        }
    }

    async fn emit(&self, event: AgentEvent) {
        self.config.hooks.emit(event, self.to()).await;
    }

    fn hook_error(&self, error: HookError) -> String {
        report_failure(error, self.to())
    }

    /// Start the next task turn unless one is in progress.
    async fn start_turn(&mut self, result: &mut LoopResult) {
        if self.turn.is_none() {
            result.turns = result.turns.saturating_add(1);
            let turn = self.control.turn_offset.saturating_add(result.turns);
            self.turn = Some(turn);
            self.emit(AgentEvent::TurnStart { turn }).await;
        }
    }

    /// End the turn in progress, if there is one.
    async fn end_turn(&mut self) {
        if let Some(turn) = self.turn.take() {
            self.emit(AgentEvent::TurnEnd { turn }).await;
        }
    }

    /// Record a finished message, then report it. Returns its entry ID in a session.
    async fn finish(&mut self, message: &Message) -> Result<Option<String>, Halt> {
        let id = match self.control.host.as_deref_mut() {
            Some(host) => Some(host.append_message(message).await.map_err(Halt::Unrecorded)?),
            None => None,
        };
        self.emit(AgentEvent::MessageEnd {
            message: message.clone(),
            entry_id: id.clone(),
        })
        .await;
        Ok(id)
    }

    async fn append(&mut self, context: &mut Vec<Message>, message: Message) -> Result<(), Halt> {
        self.emit(AgentEvent::MessageStart { role: message.role() }).await;
        self.finish(&message).await?;
        context.push(message);
        Ok(())
    }

    async fn edit(&mut self, context: &mut Vec<Message>, edit: ContextEdit) -> Result<(), Halt> {
        let (index, replacement) = match &edit {
            ContextEdit::Append { message } => return self.append(context, message.clone()).await,
            ContextEdit::ReplaceText { index, text } => (*index, Some(text.clone())),
            ContextEdit::Omit { index } => (*index, None),
        };
        let entry_id = match self.control.host.as_deref_mut() {
            Some(host) => Some(
                host.edit_context(index, replacement.as_deref())
                    .await
                    .map_err(Halt::Unrecorded)?,
            ),
            None => None,
        };
        edit.apply(context).map_err(Halt::Failed)?;
        self.emit(AgentEvent::ContextEdited {
            index,
            replacement,
            entry_id,
        })
        .await;
        Ok(())
    }

    /// Pass user input through the input hooks, then record it. The original text is kept
    /// for auditing: a rewrite or a rejection is recorded as an edit after it. A cancelled
    /// hook decided nothing, so nothing is recorded. Input from `queue` leaves the queue
    /// only once recorded, so a cancelled or dropped run leaves it for the next run.
    async fn input(&mut self, context: &mut Vec<Message>, message: Message, queue: Option<Queue>) -> Result<(), Halt> {
        let Message::User {
            content,
            kind: UserMessageKind::Prompt,
            ..
        } = &message
        else {
            return self.append(context, message).await;
        };
        let text = content.clone();
        let transformed = match self.config.hooks.on_input(text.clone(), self.trace.context()).await {
            Err(error) if error.failure == HookFailure::Cancelled => return Err(Halt::Failed(self.hook_error(error))),
            transformed => transformed,
        };
        self.append(context, message).await?;
        if let Some(queue) = queue {
            self.config.queues.remove_front(queue, &text);
        }
        let index = context.len() - 1;
        match transformed {
            Ok(replacement) if replacement != text => {
                self.edit(
                    context,
                    ContextEdit::ReplaceText {
                        index,
                        text: replacement,
                    },
                )
                .await
            }
            Ok(_) => Ok(()),
            Err(error) => {
                // A rejected input remains in the audit log but is omitted from requests.
                self.edit(context, ContextEdit::Omit { index }).await?;
                Err(Halt::Failed(self.hook_error(error)))
            }
        }
    }

    async fn run(
        &mut self,
        context: &mut Vec<Message>,
        prompts: Vec<Message>,
        result: &mut LoopResult,
    ) -> Result<LoopOutcome, Halt> {
        let config = self.config;
        config.spec.validate().map_err(Halt::Failed)?;
        self.start_turn(result).await;
        let changes = SystemPrompt::from_transcript(context).diff(&config.spec.system_prompt);
        if !changes.is_empty() {
            self.append(
                context,
                Message::System(SystemMessage {
                    sections: changes,
                    timestamp: now_ms(),
                }),
            )
            .await?;
        }
        for message in prompts {
            self.input(context, message, None).await?;
        }
        // A continuation or follow-up that a finish_turn hook asked for.
        let mut requested: Option<Message> = None;
        // Whether queued follow-ups are taken at the top of the next turn.
        let mut take_follow_ups = false;
        loop {
            // A new turn, unless a summary request has just ended inside the current one. A turn
            // exists to make a request: without budget, or once cancelled, none starts, and
            // queued input stays queued for the next run.
            if self.turn.is_none() {
                if self.control.cancel.is_cancelled() {
                    return Ok(LoopOutcome::Aborted);
                }
                if self.control.max_turns.is_some_and(|limit| result.requests >= limit) {
                    return Ok(LoopOutcome::MaxTurns);
                }
                self.start_turn(result).await;
            }
            // Queued input is taken one message at a time, so a rejected message or a stopped
            // run leaves the rest of the queue for the next run.
            while let Some(text) = config.queues.front(Queue::Steering) {
                self.input(context, Message::user(text), Some(Queue::Steering)).await?;
            }
            if std::mem::take(&mut take_follow_ups) {
                while let Some(text) = config.queues.front(Queue::FollowUp) {
                    self.input(context, Message::user(text), Some(Queue::FollowUp)).await?;
                }
            }
            if let Some(message) = requested.take() {
                self.input(context, message, None).await?;
            }
            if self.control.cancel.is_cancelled() {
                return Ok(LoopOutcome::Aborted);
            }
            if self.control.max_turns.is_some_and(|limit| result.requests >= limit) {
                return Ok(LoopOutcome::MaxTurns);
            }
            let edits = config
                .hooks
                .transform_context(context, self.trace.context())
                .await
                .map_err(|error| Halt::Failed(self.hook_error(error)))?;
            for edit in edits {
                self.edit(context, edit).await?;
            }
            if self.control.cancel.is_cancelled() {
                return Ok(LoopOutcome::Aborted);
            }
            let prepared = match self.control.host.as_deref_mut() {
                Some(host) => host.prepare_context(self.control.cancel).await,
                None => ContextPreparation::Unchanged,
            };
            let (requests, usage, failure) = match prepared {
                ContextPreparation::Unchanged => (0, Usage::default(), None),
                ContextPreparation::Updated {
                    messages,
                    requests,
                    usage,
                } => {
                    *context = messages;
                    (requests, usage, None)
                }
                ContextPreparation::Failed { error, requests, usage } => (requests, usage, Some(Halt::Failed(error))),
                ContextPreparation::Unrecorded { error, requests, usage } => {
                    (requests, usage, Some(Halt::Unrecorded(error)))
                }
            };
            // Summary requests count against the budget but are reported as a compaction,
            // not as turns of their own.
            result.usage.add(&usage);
            result.requests = result.requests.saturating_add(requests);
            if let Some(halt) = failure {
                return Err(halt);
            }
            if self.control.cancel.is_cancelled() {
                return Ok(LoopOutcome::Aborted);
            }
            if self.control.max_turns.is_some_and(|limit| result.requests >= limit) {
                return Ok(LoopOutcome::MaxTurns);
            }
            if requests > 0 {
                // Preparation is a request boundary too. Drain newly arrived steering
                // and record context hooks again before the next task request.
                continue;
            }
            let spec = &config.spec;
            let request = Request {
                model: spec.model.clone(),
                messages: context.to_vec(),
                tools: spec.tools.iter().map(|tool| tool.definition()).collect(),
                max_tokens: spec.max_tokens,
            };
            let request_entry = match self.control.host.as_deref_mut() {
                Some(host) => Some(
                    host.record_request(&request, spec.provider.as_ref())
                        .await
                        .map_err(Halt::Unrecorded)?,
                ),
                None => None,
            };
            if self.control.cancel.is_cancelled() {
                return Ok(LoopOutcome::Aborted);
            }
            self.emit(AgentEvent::RequestStart {
                kind: RequestKind::Task,
                entry_id: request_entry,
                model: spec.model.clone(),
                attempt: self.control.attempt,
            })
            .await;
            result.requests = result.requests.saturating_add(1);
            let message = self.stream(request).await;
            result.usage.add(&message.usage);
            let stop = message.stop_reason;
            let calls = message.tool_calls();
            // History owns a copy; finish_turn and accounting retain the actual response.
            let recorded = Message::Assistant(message.clone());
            let entry_id = self.finish(&recorded).await?;
            context.push(recorded);
            // The turn in progress: the loop starts one at its top and ends it below.
            let turn = self.control.turn_offset.saturating_add(result.turns);
            if matches!(stop, StopReason::Error | StopReason::Aborted) {
                self.end_turn().await;
                result.failure = Some(LoopFailure::Provider {
                    message: Box::new(message),
                    entry_id,
                });
                return Ok(if stop == StopReason::Aborted {
                    LoopOutcome::Aborted
                } else {
                    LoopOutcome::Error
                });
            }

            let mut needs_more = false;
            let mut terminated = false;
            if !calls.is_empty() {
                let batch = if stop == StopReason::Length {
                    self.truncated(&calls).await?
                } else {
                    self.tools(&calls).await?
                };
                terminated = batch.terminate;
                needs_more = !terminated;
                context.extend(batch.results.into_iter().map(Message::ToolResult));
            }
            self.end_turn().await;
            if self.control.cancel.is_cancelled() {
                return Ok(LoopOutcome::Aborted);
            }
            // Inspect queues without consuming them: stopping or failing in a hook must
            // leave accepted user input available to the next run.
            let queued = config.queues.has(Queue::Steering) || (!needs_more && config.queues.has(Queue::FollowUp));
            let disposition = if needs_more || queued {
                TurnDisposition::Continue
            } else if terminated {
                TurnDisposition::Terminated
            } else {
                TurnDisposition::Stop
            };
            let remaining_turns = self
                .control
                .max_turns
                .map(|limit| limit.saturating_sub(result.requests));
            let summary = TurnSummary {
                response: &message,
                messages: context,
                turn,
                continuations: result.continuations,
                remaining_turns,
                disposition,
            };
            let decision = config
                .hooks
                .finish_turn(&summary, self.trace.context())
                .await
                .map_err(|error| Halt::Failed(self.hook_error(error)))?;
            if decision == TurnDecision::Stop {
                return Ok(completed(stop));
            }
            // Inspect again: input may have arrived while the hook ran. It is taken at the top
            // of the next turn; follow-ups only when the model has nothing left to do.
            let steering = config.queues.has(Queue::Steering);
            take_follow_ups = !needs_more && !steering && config.queues.has(Queue::FollowUp);
            let input_waiting = steering || take_follow_ups;
            match decision {
                TurnDecision::ContinueAnswer(text) if disposition == TurnDisposition::Stop && !input_waiting => {
                    if remaining_turns == Some(0) {
                        return Ok(if stop == StopReason::Length {
                            LoopOutcome::Truncated
                        } else {
                            LoopOutcome::MaxTurns
                        });
                    }
                    result.continuations = result.continuations.saturating_add(1);
                    requested = Some(Message::continuation(text));
                }
                TurnDecision::FollowUp(text) if disposition != TurnDisposition::Continue && !input_waiting => {
                    if remaining_turns == Some(0) {
                        return Ok(LoopOutcome::MaxTurns);
                    }
                    requested = Some(Message::user(text));
                }
                TurnDecision::Stop
                | TurnDecision::Default
                | TurnDecision::ContinueAnswer(_)
                | TurnDecision::FollowUp(_) => {}
            }
            if !needs_more && !input_waiting && requested.is_none() {
                return Ok(completed(stop));
            }
        }
    }

    /// Stream one response, reporting its start and its deltas. The caller records the
    /// response and reports its end.
    async fn stream(&self, request: Request) -> AssistantMessage {
        let spec = &self.config.spec;
        let mut events = spec.provider.stream(request, self.control.cancel.clone());
        let mut started = false;
        while let Some(event) = events.next().await {
            if !started {
                started = true;
                self.emit(AgentEvent::MessageStart { role: "assistant" }).await;
            }
            match event {
                StreamEvent::Start => {}
                StreamEvent::Delta(delta) => self.emit(AgentEvent::MessageUpdate { delta }).await,
                StreamEvent::Done(message) | StreamEvent::Error(message) => return message,
            }
        }
        let stop = if self.control.cancel.is_cancelled() {
            StopReason::Aborted
        } else {
            StopReason::Error
        };
        if !started {
            self.emit(AgentEvent::MessageStart { role: "assistant" }).await;
        }
        AssistantMessage::failure(
            spec.provider.name(),
            &spec.model,
            stop,
            "provider stream ended without a final event",
        )
    }

    fn find_tool(&self, name: &str) -> Option<&'a dyn Tool> {
        self.config
            .spec
            .tools
            .iter()
            .find(|tool| tool.name() == name)
            .map(AsRef::as_ref)
    }

    async fn prepare(&mut self, call: &ToolCall) -> Prepared {
        // Resolve and validate before running policy or asking the host.
        let Some(tool) = self.find_tool(&call.name) else {
            return Prepared::blocked(format!("Tool {} not found", call.name));
        };
        if let Some(error) = &call.arguments_error {
            return Prepared::blocked(format!("Invalid arguments for tool {:?}: {error}", call.name));
        }
        if !call.arguments.is_object() {
            return Prepared::blocked(format!(
                "Invalid arguments for tool {:?}: expected a JSON object",
                call.name
            ));
        }
        let validation = std::panic::catch_unwind(AssertUnwindSafe(|| tool.validate_arguments(&call.arguments)));
        match validation {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Prepared::blocked(format!("Invalid arguments for tool {:?}: {error}", call.name)),
            Err(_) => return Prepared::blocked(format!("Tool {} argument validation panicked", call.name)),
        }
        let verdict = match self.config.hooks.before_tool(call, self.trace.context()).await {
            Ok(verdict) => verdict,
            Err(error) => {
                let reason = self.hook_error(error);
                self.decision(call, ToolVerdict::Deny, Some(reason.clone())).await;
                return Prepared::blocked(format!("Blocked by policy: {reason}"));
            }
        };
        match verdict {
            Decision::Allow => self.decision(call, ToolVerdict::Allow, None).await,
            Decision::Deny(reason) => {
                self.decision(call, ToolVerdict::Deny, Some(reason.clone())).await;
                return Prepared::blocked(format!("Blocked by policy: {reason}"));
            }
            Decision::Ask(reason) => {
                self.decision(call, ToolVerdict::Ask, Some(reason.clone())).await;
                if self.control.approvals.allows(call) {
                    self.approval_resolved(call, ApprovalResult::Remembered, None, 0).await;
                } else if let Err(denied) = self.ask(call, reason).await {
                    return Prepared::blocked(format!("Approval denied: {denied}"));
                }
            }
        }
        if self.control.cancel.is_cancelled() {
            return Prepared::blocked(NOT_STARTED);
        }
        // Tool storage is static; the execution phase can resolve it again by name.
        Prepared::Ready
    }

    /// Ask the interaction host to approve `call`. Returns why the call may not run.
    async fn ask(&mut self, call: &ToolCall, reason: String) -> Result<(), String> {
        self.emit(AgentEvent::ApprovalRequested {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            reason: reason.clone(),
        })
        .await;
        let request = ApprovalRequest {
            call: call.clone(),
            reason,
        };
        let guard = HookCall {
            name: "Interaction::confirm",
            phase: HookPhase::Approval,
            timeout: self.config.interaction_timeout,
        };
        let asked = Instant::now();
        let answer = guard
            .run(self.config.interaction.confirm(&request), self.control.cancel)
            .await;
        let waited = duration_ms(asked.elapsed());
        match answer {
            Ok(Approval::AllowOnce) => {
                self.approval_resolved(call, ApprovalResult::AllowOnce, None, waited)
                    .await;
                Ok(())
            }
            Ok(Approval::AllowSession) => {
                self.control.approvals.remember(call);
                self.approval_resolved(call, ApprovalResult::AllowSession, None, waited)
                    .await;
                Ok(())
            }
            Ok(Approval::Deny(reason)) => {
                self.approval_resolved(call, ApprovalResult::Denied, Some(reason.clone()), waited)
                    .await;
                Err(reason)
            }
            Err(error) => {
                let result = match error.failure {
                    HookFailure::TimedOut(_) => ApprovalResult::TimedOut,
                    HookFailure::Cancelled => ApprovalResult::Cancelled,
                    HookFailure::Rejected(_) | HookFailure::Panicked => ApprovalResult::Failed,
                };
                let reason = self.hook_error(error);
                self.approval_resolved(call, result, Some(reason.clone()), waited).await;
                Err(reason)
            }
        }
    }

    async fn decision(&self, call: &ToolCall, verdict: ToolVerdict, reason: Option<String>) {
        self.emit(AgentEvent::ToolDecision {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            verdict,
            reason,
        })
        .await;
    }

    async fn approval_resolved(&self, call: &ToolCall, approval: ApprovalResult, reason: Option<String>, wait_ms: u64) {
        self.emit(AgentEvent::ApprovalResolved {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            approval,
            reason,
            wait_ms,
        })
        .await;
    }

    async fn tools(&mut self, calls: &[ToolCall]) -> Result<Batch, Halt> {
        let sequential = self.config.spec.sequential_tools
            || calls.iter().any(|call| {
                self.find_tool(&call.name)
                    .is_some_and(|tool| tool.execution_mode() == ExecutionMode::Sequential)
            });
        if sequential {
            self.sequential(calls).await
        } else {
            self.parallel(calls).await
        }
    }

    async fn sequential(&mut self, calls: &[ToolCall]) -> Result<Batch, Halt> {
        let mut results = Vec::new();
        let mut terminate = true;
        for call in calls {
            self.tool_start(call).await;
            let outcome = match self.prepare(call).await {
                Prepared::Immediate(outcome) => outcome,
                Prepared::Ready => {
                    let tool = self.find_tool(&call.name).expect("tool was resolved above");
                    self.record_start(call, tool.effects()).await?;
                    execute(tool, call.arguments.clone(), &self.config.spec, self.control.cancel).await
                }
            };
            let outcome = self.after(call, outcome).await;
            self.tool_end(call, &outcome).await;
            terminate &= outcome.output.terminate;
            let result = to_result(call, outcome);
            self.finish_result(&result).await?;
            results.push(result);
            if self.control.cancel.is_cancelled() {
                break;
            }
        }
        Ok(Batch {
            terminate: terminate && !results.is_empty(),
            results,
        })
    }

    async fn parallel(&mut self, calls: &[ToolCall]) -> Result<Batch, Halt> {
        // Phase 1: all checks are ordered. Store verdicts, then borrow the static
        // tools during execution after the mutable approval borrow ends.
        let mut slots = Vec::new();
        for call in calls {
            self.tool_start(call).await;
            let immediate = match self.prepare(call).await {
                Prepared::Immediate(outcome) => Some(outcome),
                Prepared::Ready => None,
            };
            slots.push((call, immediate));
            if self.control.cancel.is_cancelled() {
                break;
            }
        }
        // Record starts in call order before any call runs, so that recovery after a crash
        // can tell the calls that began from those that never did.
        for (call, immediate) in &slots {
            if immediate.is_none() {
                let tool = self.find_tool(&call.name).expect("tool was resolved in phase 1");
                self.record_start(call, tool.effects()).await?;
            }
        }
        // Phase 2: borrow the runtime; futures do not need spawn or shared mutable state.
        let engine = &*self;
        let outcomes = join_all(slots.into_iter().map(|(call, immediate)| async move {
            let outcome = match immediate {
                Some(outcome) => outcome,
                None => {
                    execute(
                        engine.find_tool(&call.name).expect("tool was resolved in phase 1"),
                        call.arguments.clone(),
                        &engine.config.spec,
                        engine.control.cancel,
                    )
                    .await
                }
            };
            let outcome = engine.after(call, outcome).await;
            engine.tool_end(call, &outcome).await;
            (call, outcome)
        }))
        .await;
        // Phase 3: record results in model call order, regardless of completion order.
        let terminate = !outcomes.is_empty() && outcomes.iter().all(|(_, result)| result.output.terminate);
        let mut results = Vec::with_capacity(outcomes.len());
        for (call, outcome) in outcomes {
            let result = to_result(call, outcome);
            self.finish_result(&result).await?;
            results.push(result);
        }
        Ok(Batch { results, terminate })
    }

    /// Record the start of a call with side effects; an error means it must not run.
    /// Read-only calls are not recorded.
    async fn record_start(&mut self, call: &ToolCall, effects: Effects) -> Result<(), Halt> {
        match (effects, self.control.host.as_deref_mut()) {
            (Effects::SideEffects, Some(host)) => host.record_tool_call_started(call).await.map_err(Halt::Unrecorded),
            (Effects::SideEffects, None) | (Effects::ReadOnly, _) => Ok(()),
        }
    }

    async fn after(&self, call: &ToolCall, outcome: Outcome) -> Outcome {
        let output = match self
            .config
            .hooks
            .after_tool(call, outcome.output, self.trace.context())
            .await
        {
            Ok(output) => output,
            // Discard the raw output on every failure, including cancellation.
            Err(error) => ToolOutput::error(format!("Tool output withheld: {}", self.hook_error(error))),
        };
        Outcome {
            output,
            duration_ms: outcome.duration_ms,
        }
    }

    async fn truncated(&mut self, calls: &[ToolCall]) -> Result<Batch, Halt> {
        let mut results = Vec::new();
        for call in calls {
            self.tool_start(call).await;
            let outcome = self.after(call, Outcome::blocked(format!(
                "Tool call {:?} was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                call.name
            ))).await;
            self.tool_end(call, &outcome).await;
            let result = to_result(call, outcome);
            self.finish_result(&result).await?;
            results.push(result);
        }
        Ok(Batch {
            results,
            terminate: false,
        })
    }

    async fn tool_start(&self, call: &ToolCall) {
        self.emit(AgentEvent::ToolExecutionStart {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            args: call.arguments.clone(),
        })
        .await;
    }

    async fn tool_end(&self, call: &ToolCall, outcome: &Outcome) {
        self.emit(AgentEvent::ToolExecutionEnd {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            is_error: outcome.output.is_error,
            duration_ms: outcome.duration_ms,
        })
        .await;
    }

    async fn finish_result(&mut self, result: &ToolResultMessage) -> Result<(), Halt> {
        self.emit(AgentEvent::MessageStart { role: "toolResult" }).await;
        self.finish(&Message::ToolResult(result.clone())).await?;
        Ok(())
    }
}

fn completed(stop: StopReason) -> LoopOutcome {
    if stop == StopReason::Length {
        LoopOutcome::Truncated
    } else {
        LoopOutcome::Completed
    }
}

struct Batch {
    results: Vec<ToolResultMessage>,
    terminate: bool,
}

struct Outcome {
    output: ToolOutput,
    duration_ms: Option<u64>,
}

impl Outcome {
    fn blocked(text: impl Into<String>) -> Self {
        Self {
            output: ToolOutput::error(text),
            duration_ms: None,
        }
    }
}

enum Prepared {
    Immediate(Outcome),
    Ready,
}

impl Prepared {
    fn blocked(text: impl Into<String>) -> Self {
        Self::Immediate(Outcome::blocked(text))
    }
}

/// The result of a call that never started because the run was cancelled first.
const NOT_STARTED: &str = "Not executed: the run was cancelled before this call started, so it had no effect.";

/// The result of a call that the cancellation interrupted. Like a call that a crash
/// interrupted, one with side effects may have changed state before it stopped.
fn interrupted(effects: Effects) -> &'static str {
    match effects {
        Effects::SideEffects => {
            "Interrupted: the run was cancelled while this call was running. It may have changed state; check the current state before running it again."
        }
        Effects::ReadOnly => {
            "Interrupted: the run was cancelled while this read-only call was running. It changed nothing; run it again if the result is still needed."
        }
    }
}

async fn execute(tool: &dyn Tool, args: Value, spec: &LoopSpec, cancel: &CancellationToken) -> Outcome {
    if cancel.is_cancelled() {
        return Outcome::blocked(NOT_STARTED);
    }
    let ctx = ToolContext {
        cwd: spec.cwd.clone(),
        cancel: cancel.clone(),
    };
    let started = Instant::now();
    let future = AssertUnwindSafe(tool.execute(args, &ctx)).catch_unwind();
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            return Outcome {
                output: ToolOutput::error(interrupted(tool.effects())),
                duration_ms: Some(duration_ms(started.elapsed())),
            };
        }
        result = future => result,
    };
    let output = match result {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => ToolOutput::error(error),
        Err(_) => ToolOutput::error(format!("Tool {} panicked", tool.name())),
    };
    Outcome {
        output,
        duration_ms: Some(duration_ms(started.elapsed())),
    }
}

fn to_result(call: &ToolCall, outcome: Outcome) -> ToolResultMessage {
    let Outcome { output, duration_ms } = outcome;
    let content = match (output.content.is_empty(), output.is_error) {
        (false, _) => output.content,
        (true, true) => "(tool failed without output)".to_string(),
        (true, false) => "(no output)".to_string(),
    };
    ToolResultMessage {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        content,
        is_error: output.is_error,
        timestamp: now_ms(),
        duration_ms,
    }
}
