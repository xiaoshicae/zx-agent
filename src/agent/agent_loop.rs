//! The agent loop. Extension behavior is supplied through bounded, ordered hooks.

use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::join_all;
use futures::{FutureExt, StreamExt};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::events::{AgentEvent, ContextPreparation, EventSink, LoopOutcome};
use super::hooks::{
    ContextEdit, HookCall, HookChain, HookError, HookFailure, HookPhase, TurnDecision, TurnDisposition, TurnSummary,
    report_failure,
};
use super::policy::Decision;
use super::queue::MessageQueues;
use super::tool::{ExecutionMode, Tool, ToolContext, ToolOutput};
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
    pub max_turns: Option<u32>,
    pub turn_offset: u32,
    pub continuations: u32,
    pub approvals: &'a mut ApprovalCache,
    pub cancel: &'a CancellationToken,
    pub sink: &'a dyn EventSink,
}

impl<'a> LoopControl<'a> {
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
        }
    }
}

/// Accounting follows actual responses, so context edits cannot change usage or retries.
#[derive(Debug)]
pub enum LoopFailure {
    /// Boxed so a successful LoopResult does not reserve space for a full response.
    Provider(Box<AssistantMessage>),
    Host(String),
}

/// Result of one loop attempt. Only provider failures qualify for provider retries.
#[derive(Debug)]
pub struct LoopResult {
    pub outcome: LoopOutcome,
    pub turns: u32,
    pub continuations: u32,
    pub usage: Usage,
    pub failure: Option<LoopFailure>,
}

/// Append prompts and run until the model stops, the budget expires, or cancellation.
/// Hooks persist changes before a request; the provider sees exactly this projection.
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
        continuations: control.continuations,
        usage: Usage::default(),
        failure: None,
    };
    let mut engine = Engine { config, control };
    engine.emit(AgentEvent::AgentStart).await;
    match engine.run(context, prompts, &mut result).await {
        Ok(outcome) => result.outcome = outcome,
        Err(error) => {
            result.outcome = if engine.control.cancel.is_cancelled() {
                LoopOutcome::Aborted
            } else {
                LoopOutcome::Error
            };
            result.failure = Some(LoopFailure::Host(error));
        }
    }
    engine
        .emit(AgentEvent::AgentEnd {
            outcome: result.outcome,
        })
        .await;
    result
}

struct Engine<'a, 'b, 'c> {
    config: &'a LoopConfig,
    control: &'b mut LoopControl<'c>,
}

impl Engine<'_, '_, '_> {
    async fn emit(&self, event: AgentEvent) {
        self.config
            .hooks
            .emit(event, self.control.cancel, self.control.sink)
            .await;
    }

    fn hook_error(&self, error: HookError) -> String {
        report_failure(error, self.control.sink)
    }

    async fn append(&self, context: &mut Vec<Message>, message: Message) {
        self.emit(AgentEvent::MessageStart { role: message.role() }).await;
        self.emit(AgentEvent::MessageEnd {
            message: message.clone(),
        })
        .await;
        context.push(message);
    }

    async fn edit(&self, context: &mut Vec<Message>, edit: ContextEdit) -> Result<(), String> {
        match &edit {
            ContextEdit::Append(message) => self.append(context, message.clone()).await,
            ContextEdit::ReplaceText { index, text } => {
                self.control.sink.edit_context(*index, Some(text))?;
                edit.apply(context)?;
                self.emit(AgentEvent::ContextEdited {
                    index: *index,
                    replacement: Some(text.clone()),
                })
                .await;
            }
            ContextEdit::Omit { index } => {
                self.control.sink.edit_context(*index, None)?;
                edit.apply(context)?;
                self.emit(AgentEvent::ContextEdited {
                    index: *index,
                    replacement: None,
                })
                .await;
            }
        }
        Ok(())
    }

    async fn input(&self, context: &mut Vec<Message>, message: Message) -> Result<(), String> {
        self.append(context, message).await;
        let index = context.len() - 1;
        let Message::User {
            content,
            kind: UserMessageKind::Prompt,
            ..
        } = &context[index]
        else {
            return Ok(());
        };
        let transformed = self.config.hooks.on_input(content.clone(), self.control.cancel).await;
        match transformed {
            Ok(text) if text != *content => {
                self.edit(context, ContextEdit::ReplaceText { index, text }).await?;
            }
            Ok(_) => {}
            Err(error) => {
                // A rejected input remains in the audit log but is omitted from requests.
                if error.failure != HookFailure::Cancelled {
                    self.edit(context, ContextEdit::Omit { index }).await?;
                }
                return Err(self.hook_error(error));
            }
        }
        Ok(())
    }

    async fn run(
        &mut self,
        context: &mut Vec<Message>,
        prompts: Vec<Message>,
        result: &mut LoopResult,
    ) -> Result<LoopOutcome, String> {
        self.emit(AgentEvent::TurnStart {
            turn: self.control.turn_offset.saturating_add(1),
        })
        .await;
        let changes = SystemPrompt::from_transcript(context).diff(&self.config.spec.system_prompt);
        if !changes.is_empty() {
            self.append(
                context,
                Message::System(SystemMessage {
                    sections: changes,
                    timestamp: now_ms(),
                }),
            )
            .await;
        }
        for message in prompts {
            self.input(context, message).await?;
        }
        let mut pending = self.config.queues.drain_steering();
        loop {
            if result.turns > 0 {
                if pending.is_empty() {
                    pending = self.config.queues.drain_steering();
                }
                self.emit(AgentEvent::TurnStart {
                    turn: self.control.turn_offset.saturating_add(result.turns).saturating_add(1),
                })
                .await;
            }
            for message in pending.drain(..) {
                self.input(context, message).await?;
            }
            if self.control.cancel.is_cancelled() {
                return Ok(LoopOutcome::Aborted);
            }
            if self.control.max_turns.is_some_and(|limit| result.turns >= limit) {
                return Ok(LoopOutcome::MaxTurns);
            }
            let edits = self
                .config
                .hooks
                .transform_context(context, self.control.cancel)
                .await
                .map_err(|error| self.hook_error(error))?;
            for edit in edits {
                self.edit(context, edit).await?;
            }
            if self.control.cancel.is_cancelled() {
                return Ok(LoopOutcome::Aborted);
            }
            let prepared = self.control.sink.prepare_context(self.control.cancel).await;
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
                ContextPreparation::Failed { error, requests, usage } => (requests, usage, Some(error)),
            };
            result.usage.add(&usage);
            for request in 0..requests {
                result.turns = result.turns.saturating_add(1);
                self.emit(AgentEvent::TurnEnd {
                    turn: self.control.turn_offset.saturating_add(result.turns),
                })
                .await;
                if request + 1 < requests {
                    self.emit(AgentEvent::TurnStart {
                        turn: self.control.turn_offset.saturating_add(result.turns).saturating_add(1),
                    })
                    .await;
                }
            }
            if let Some(error) = failure {
                return Err(error);
            }
            if self.control.cancel.is_cancelled() {
                return Ok(LoopOutcome::Aborted);
            }
            if self.control.max_turns.is_some_and(|limit| result.turns >= limit) {
                return Ok(LoopOutcome::MaxTurns);
            }
            if requests > 0 {
                // Preparation is a request boundary too. Drain newly arrived steering
                // and persist context hooks again before the next task request.
                continue;
            }
            let spec = &self.config.spec;
            let request = Request {
                model: spec.model.clone(),
                messages: context.to_vec(),
                tools: spec.tools.iter().map(|tool| tool.definition()).collect(),
                max_tokens: spec.max_tokens,
            };
            self.control.sink.record_request(&request, spec.provider.as_ref())?;
            if self.control.cancel.is_cancelled() {
                return Ok(LoopOutcome::Aborted);
            }
            result.turns = result.turns.saturating_add(1);
            let message = self.stream(request).await;
            result.usage.add(&message.usage);
            let stop = message.stop_reason;
            let calls = message.tool_calls();
            // History owns a copy; finish_turn and accounting retain the actual response.
            context.push(Message::Assistant(message.clone()));
            let turn = self.control.turn_offset.saturating_add(result.turns);
            if matches!(stop, StopReason::Error | StopReason::Aborted) {
                self.emit(AgentEvent::TurnEnd { turn }).await;
                result.failure = Some(LoopFailure::Provider(Box::new(message)));
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
                    self.truncated(&calls).await
                } else {
                    self.tools(&calls).await
                };
                terminated = batch.terminate;
                needs_more = !terminated;
                context.extend(batch.results.into_iter().map(Message::ToolResult));
            }
            self.emit(AgentEvent::TurnEnd { turn }).await;
            if self.control.cancel.is_cancelled() {
                return Ok(LoopOutcome::Aborted);
            }
            // Inspect queues without consuming them: stopping or failing in a hook must
            // leave accepted user input available to the next run.
            let queued = self.config.queues.has_steering() || (!needs_more && self.config.queues.has_follow_up());
            let disposition = if needs_more || queued {
                TurnDisposition::Continue
            } else if terminated {
                TurnDisposition::Terminated
            } else {
                TurnDisposition::Stop
            };
            let remaining_turns = self.control.max_turns.map(|limit| limit.saturating_sub(result.turns));
            let summary = TurnSummary {
                response: &message,
                messages: context,
                turn,
                continuations: result.continuations,
                remaining_turns,
                disposition,
            };
            let decision = self
                .config
                .hooks
                .finish_turn(&summary, self.control.cancel)
                .await
                .map_err(|error| self.hook_error(error))?;
            if decision == TurnDecision::Stop {
                return Ok(completed(stop));
            }
            pending = self.config.queues.drain_steering();
            if !needs_more && pending.is_empty() {
                pending = self.config.queues.drain_follow_up();
            }
            match decision {
                TurnDecision::ContinueAnswer(text) if disposition == TurnDisposition::Stop && pending.is_empty() => {
                    if remaining_turns == Some(0) {
                        return Ok(if stop == StopReason::Length {
                            LoopOutcome::Truncated
                        } else {
                            LoopOutcome::MaxTurns
                        });
                    }
                    result.continuations = result.continuations.saturating_add(1);
                    pending.push(Message::continuation(text));
                }
                TurnDecision::FollowUp(text) if disposition != TurnDisposition::Continue && pending.is_empty() => {
                    if remaining_turns == Some(0) {
                        return Ok(LoopOutcome::MaxTurns);
                    }
                    pending.push(Message::user(text));
                }
                TurnDecision::Stop
                | TurnDecision::Default
                | TurnDecision::ContinueAnswer(_)
                | TurnDecision::FollowUp(_) => {}
            }
            if !needs_more && pending.is_empty() {
                return Ok(completed(stop));
            }
        }
    }

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
                StreamEvent::Done(message) | StreamEvent::Error(message) => {
                    self.emit(AgentEvent::MessageEnd {
                        message: Message::Assistant(message.clone()),
                    })
                    .await;
                    return message;
                }
            }
        }
        let stop = if self.control.cancel.is_cancelled() {
            StopReason::Aborted
        } else {
            StopReason::Error
        };
        let message = AssistantMessage::failure(
            spec.provider.name(),
            &spec.model,
            stop,
            "provider stream ended without a final event",
        );
        if !started {
            self.emit(AgentEvent::MessageStart { role: "assistant" }).await;
        }
        self.emit(AgentEvent::MessageEnd {
            message: Message::Assistant(message.clone()),
        })
        .await;
        message
    }

    fn find_tool(&self, name: &str) -> Option<&dyn Tool> {
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
        let verdict = match self.config.hooks.before_tool(call, self.control.cancel).await {
            Ok(verdict) => verdict,
            Err(error) => return Prepared::blocked(format!("Blocked by policy: {}", self.hook_error(error))),
        };
        match verdict {
            Decision::Deny(reason) => return Prepared::blocked(format!("Blocked by policy: {reason}")),
            Decision::Ask(reason) if !self.control.approvals.allows(call) => {
                let request = ApprovalRequest {
                    call: call.clone(),
                    reason,
                };
                let guard = HookCall {
                    name: "Interaction::confirm",
                    phase: HookPhase::Approval,
                    timeout: self.config.interaction_timeout,
                };
                match guard
                    .run(self.config.interaction.confirm(&request), self.control.cancel)
                    .await
                {
                    Ok(Approval::AllowOnce) => {}
                    Ok(Approval::AllowSession) => self.control.approvals.remember(call),
                    Ok(Approval::Deny(reason)) => return Prepared::blocked(format!("Approval denied: {reason}")),
                    Err(error) => return Prepared::blocked(format!("Approval denied: {}", self.hook_error(error))),
                }
            }
            Decision::Allow | Decision::Ask(_) => {}
        }
        if self.control.cancel.is_cancelled() {
            return Prepared::blocked("Operation aborted");
        }
        // Tool storage is static; the execution phase can resolve it again by name.
        Prepared::Ready
    }

    async fn tools(&mut self, calls: &[ToolCall]) -> Batch {
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

    async fn sequential(&mut self, calls: &[ToolCall]) -> Batch {
        let mut results = Vec::new();
        let mut terminate = true;
        for call in calls {
            self.tool_start(call).await;
            // Keep only the preparation verdict so the mutable approval borrow ends here.
            let outcome = match self.prepare(call).await {
                Prepared::Immediate(outcome) => outcome,
                Prepared::Ready => {
                    execute(
                        self.find_tool(&call.name).expect("tool was resolved above"),
                        call.arguments.clone(),
                        &self.config.spec,
                        self.control.cancel,
                    )
                    .await
                }
            };
            let outcome = self.after(call, outcome).await;
            self.tool_end(call, &outcome).await;
            terminate &= outcome.output.terminate;
            let result = to_result(call, outcome);
            self.emit_result(&result).await;
            results.push(result);
            if self.control.cancel.is_cancelled() {
                break;
            }
        }
        Batch {
            terminate: terminate && !results.is_empty(),
            results,
        }
    }

    async fn parallel(&mut self, calls: &[ToolCall]) -> Batch {
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
        // Phase 3: commit results in model call order, regardless of completion order.
        let terminate = !outcomes.is_empty() && outcomes.iter().all(|(_, result)| result.output.terminate);
        let mut results = Vec::with_capacity(outcomes.len());
        for (call, outcome) in outcomes {
            let result = to_result(call, outcome);
            self.emit_result(&result).await;
            results.push(result);
        }
        Batch { results, terminate }
    }

    async fn after(&self, call: &ToolCall, outcome: Outcome) -> Outcome {
        let output = match self
            .config
            .hooks
            .after_tool(call, outcome.output, self.control.cancel)
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

    async fn truncated(&self, calls: &[ToolCall]) -> Batch {
        let mut results = Vec::new();
        for call in calls {
            self.tool_start(call).await;
            let outcome = self.after(call, Outcome::blocked(format!(
                "Tool call {:?} was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                call.name
            ))).await;
            self.tool_end(call, &outcome).await;
            let result = to_result(call, outcome);
            self.emit_result(&result).await;
            results.push(result);
        }
        Batch {
            results,
            terminate: false,
        }
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

    async fn emit_result(&self, result: &ToolResultMessage) {
        self.emit(AgentEvent::MessageStart { role: "toolResult" }).await;
        self.emit(AgentEvent::MessageEnd {
            message: Message::ToolResult(result.clone()),
        })
        .await;
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

async fn execute(tool: &dyn Tool, args: Value, spec: &LoopSpec, cancel: &CancellationToken) -> Outcome {
    if cancel.is_cancelled() {
        return Outcome::blocked("Operation aborted");
    }
    let ctx = ToolContext {
        cwd: spec.cwd.clone(),
        cancel: cancel.clone(),
    };
    let started = Instant::now();
    let future = AssertUnwindSafe(tool.execute(args, &ctx)).catch_unwind();
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Outcome::blocked("Operation aborted"),
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
