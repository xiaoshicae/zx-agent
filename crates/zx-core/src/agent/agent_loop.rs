//! The agent loop. Extension behavior is supplied through bounded, ordered hooks.

mod context;
mod model;
mod tools;

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::join_all;
use futures::{FutureExt, StreamExt};
use tokio_util::sync::CancellationToken;

use super::events::{AgentEvent, ApprovalResult, Emit, EventSink, LoopOutcome, RequestKind, RunTrace, ToolVerdict};
use super::hooks::{
    ContextEdit, HookCall, HookChain, HookError, HookFailure, HookPhase, TurnDecision, TurnDisposition, TurnSummary,
    report_failure,
};
use super::policy::Decision;
use super::queue::{MessageQueues, Queue};
use super::session_runtime::{ContextPreparation, SessionRuntime};
use super::tool::{Effects, ExecutionMode, MAX_DETAILS_BYTES, Tool, ToolContext, ToolOutput};
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
    /// Check what the loop relies on: the model calls tools by the names they declare, so
    /// each tool must declare the name it is called by and no two may share one. A tool's
    /// `name`, `definition`, `execution_mode` and `effects` must not panic; they are tried
    /// here, so that a panic fails the run before its first request instead of escaping it.
    /// `run_loop` checks before its first request.
    pub fn validate(&self) -> Result<(), String> {
        self.validators().map(|_| ())
    }

    fn validators(&self) -> Result<HashMap<String, jsonschema::Validator>, String> {
        let mut validators = HashMap::new();
        for (index, tool) in self.tools.iter().enumerate() {
            let described = std::panic::catch_unwind(AssertUnwindSafe(|| {
                (
                    tool.name().to_string(),
                    tool.definition(),
                    tool.execution_mode(),
                    tool.effects(),
                )
            }));
            let Ok((name, definition, _, _)) = described else {
                return Err(format!("tool {index} panicked while describing itself"));
            };
            if definition.name != name {
                return Err(format!(
                    "tool {name:?} declares itself as {:?}; the model calls a tool by the name it declares",
                    definition.name
                ));
            }
            if validators.contains_key(&name) {
                return Err(format!("more than one tool is named {name:?}"));
            }
            let validator = jsonschema::validator_for(&definition.parameters)
                .map_err(|error| format!("tool {name:?} has an invalid parameter schema: {error}"))?;
            validators.insert(name, validator);
        }
        Ok(validators)
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
    pub max_model_requests: Option<u32>,
    /// Tokens this attempt may use, input plus output as the provider reports them, summary
    /// requests included. Checked before each request, so the last one may go over; `None`
    /// is unlimited.
    pub token_budget: Option<u64>,
    /// Task turns of earlier attempts, so that turn numbers continue across retries.
    pub prior_task_turns: u32,
    /// Answer continuations scheduled in earlier attempts of this run.
    pub prior_answer_continuations: u32,
    pub approvals: &'a mut ApprovalCache,
    pub cancel: &'a CancellationToken,
    pub sink: &'a dyn EventSink,
    /// Records each step before the loop acts on it; the Runner supplies its session. Without
    /// one, the loop keeps messages and edits only in its context.
    pub(crate) session: Option<&'a mut dyn SessionRuntime>,
    /// The run this attempt belongs to; without one, `run_loop` is a run of its own.
    pub(crate) trace: Option<&'a RunTrace>,
    /// 1 for the first attempt of a run, plus one for each retry.
    pub(crate) attempt_number: u32,
}

impl<'a> LoopControl<'a> {
    /// Control for a run that keeps its messages only in the context passed to `run_loop`.
    pub fn new(
        max_model_requests: Option<u32>,
        approvals: &'a mut ApprovalCache,
        cancel: &'a CancellationToken,
        sink: &'a dyn EventSink,
    ) -> Self {
        Self {
            max_model_requests,
            token_budget: None,
            prior_task_turns: 0,
            prior_answer_continuations: 0,
            approvals,
            cancel,
            sink,
            session: None,
            trace: None,
            attempt_number: 1,
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
    /// Task turns started, numbered from `prior_task_turns + 1`.
    pub task_turns: u32,
    /// Model requests made, summary requests included: what the budget counts.
    pub model_requests: u32,
    /// Total answer continuations scheduled in this run, including prior attempts.
    pub answer_continuations: u32,
    pub usage: Usage,
    pub failure: Option<LoopFailure>,
}

/// Append prompts and run until the model stops, the budget expires, or cancellation.
/// Each step is recorded before the loop acts on it; the provider sees exactly this context.
/// A request budget of `None` is unlimited; `Some(0)` makes no model request.
pub async fn run_loop(
    context: &mut Vec<Message>,
    prompts: Vec<Message>,
    config: &LoopConfig,
    control: &mut LoopControl<'_>,
) -> LoopResult {
    let mut result = LoopResult {
        outcome: LoopOutcome::Completed,
        task_turns: 0,
        model_requests: 0,
        answer_continuations: control.prior_answer_continuations,
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
        validators: HashMap::new(),
    };

    let attempt = engine.control.attempt_number;
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
    validators: HashMap<String, jsonschema::Validator>,
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
            result.task_turns = result.task_turns.saturating_add(1);
            let turn = self.control.prior_task_turns.saturating_add(result.task_turns);
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

    /// Validate the specification and record the initial system prompt and input.
    async fn initialize(
        &mut self,
        context: &mut Vec<Message>,
        prompts: Vec<Message>,
        result: &mut LoopResult,
    ) -> Result<(), Halt> {
        let config = self.config;
        self.validators = config.spec.validators().map_err(Halt::Failed)?;
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
        Ok(())
    }

    async fn run(
        &mut self,
        context: &mut Vec<Message>,
        prompts: Vec<Message>,
        result: &mut LoopResult,
    ) -> Result<LoopOutcome, Halt> {
        self.initialize(context, prompts, result).await?;
        let mut pending = PendingInput::default();

        // main loop
        loop {
            match self.prepare_turn(context, result, &mut pending).await? {
                TurnPreparation::Ready => {}
                TurnPreparation::Retry => continue,
                TurnPreparation::Stop(outcome) => return Ok(outcome),
            }
            let Some(message) = self.request_model(context, result).await? else {
                return Ok(LoopOutcome::Aborted);
            };
            let response = match self.handle_response(context, result, message).await? {
                ResponseHandling::Stop(outcome) => return Ok(outcome),
                ResponseHandling::Continue(response) => response,
            };
            if let Some(outcome) = self.decide_next_turn(context, result, &response, &mut pending).await? {
                return Ok(outcome);
            }
        }
    }

    /// Record the response, execute its tool calls, and close the turn.
    async fn handle_response(
        &mut self,
        context: &mut Vec<Message>,
        result: &mut LoopResult,
        message: AssistantMessage,
    ) -> Result<ResponseHandling, Halt> {
        let stop = message.stop_reason;
        let calls = message.tool_calls();
        // History owns a copy; finish_turn and accounting retain the actual response.
        let recorded = Message::Assistant(message.clone());
        let entry_id = self.record_finished_message(&recorded).await?;
        context.push(recorded);
        // The turn in progress: the loop starts one at its top and ends it below.
        let turn = self.control.prior_task_turns.saturating_add(result.task_turns);
        if matches!(stop, StopReason::Error | StopReason::Aborted) {
            self.end_turn().await;
            result.failure = Some(LoopFailure::Provider {
                message: Box::new(message),
                entry_id,
            });
            return Ok(ResponseHandling::Stop(if stop == StopReason::Aborted {
                LoopOutcome::Aborted
            } else {
                LoopOutcome::Error
            }));
        }

        let mut tool_progress = ToolProgress::NoCalls;
        if !calls.is_empty() {
            let batch = if stop == StopReason::Length {
                self.reject_truncated_tools(&calls).await?
            } else {
                self.execute_tools(&calls).await?
            };
            tool_progress = if batch.terminate {
                ToolProgress::Terminated
            } else {
                ToolProgress::Continue
            };
            context.extend(batch.results.into_iter().map(Message::ToolResult));
        }
        self.end_turn().await;
        if self.control.cancel.is_cancelled() {
            return Ok(ResponseHandling::Stop(LoopOutcome::Aborted));
        }
        Ok(ResponseHandling::Continue(Box::new(TurnResponse {
            message,
            turn,
            tool_progress,
        })))
    }

    /// Ask the finish hook and decide whether another turn is needed.
    async fn decide_next_turn(
        &mut self,
        context: &[Message],
        result: &mut LoopResult,
        response: &TurnResponse,
        pending: &mut PendingInput,
    ) -> Result<Option<LoopOutcome>, Halt> {
        let config = self.config;
        let TurnResponse {
            message,
            turn,
            tool_progress,
        } = response;
        let turn = *turn;
        let needs_more = *tool_progress == ToolProgress::Continue;
        let stop = message.stop_reason;
        // Inspect queues without consuming them: stopping or failing in a hook must
        // leave accepted user input available to the next run.
        let queued = config.queues.has(Queue::Steering) || (!needs_more && config.queues.has(Queue::FollowUp));
        let disposition = if needs_more || queued {
            TurnDisposition::Continue
        } else if *tool_progress == ToolProgress::Terminated {
            TurnDisposition::Terminated
        } else {
            TurnDisposition::Stop
        };
        let remaining_model_requests = self
            .control
            .max_model_requests
            .map(|limit| limit.saturating_sub(result.model_requests));
        let summary = TurnSummary {
            response: message,
            messages: context,
            turn,
            answer_continuations: result.answer_continuations,
            remaining_model_requests,
            disposition,
        };
        let decision = config
            .hooks
            .finish_turn(&summary, self.trace.context())
            .await
            .map_err(|error| Halt::Failed(self.hook_error(error)))?;
        if decision == TurnDecision::Stop {
            return Ok(Some(completed(stop)));
        }
        // Inspect again: input may have arrived while the hook ran. It is taken at the top
        // of the next turn; follow-ups only when the model has nothing left to do.
        let steering = config.queues.has(Queue::Steering);
        pending.take_follow_ups = !needs_more && !steering && config.queues.has(Queue::FollowUp);
        let input_waiting = steering || pending.take_follow_ups;
        match decision {
            TurnDecision::ContinueAnswer(text) if disposition == TurnDisposition::Stop && !input_waiting => {
                if remaining_model_requests == Some(0) {
                    return Ok(Some(if stop == StopReason::Length {
                        LoopOutcome::Truncated
                    } else {
                        LoopOutcome::ModelRequestLimit
                    }));
                }
                result.answer_continuations = result.answer_continuations.saturating_add(1);
                pending.requested = Some(Message::continuation(text));
            }
            TurnDecision::FollowUp(text) if disposition != TurnDisposition::Continue && !input_waiting => {
                if remaining_model_requests == Some(0) {
                    return Ok(Some(LoopOutcome::ModelRequestLimit));
                }
                pending.requested = Some(Message::user(text));
            }
            TurnDecision::Stop
            | TurnDecision::Default
            | TurnDecision::ContinueAnswer(_)
            | TurnDecision::FollowUp(_) => {}
        }
        if !needs_more && !input_waiting && pending.requested.is_none() {
            return Ok(Some(completed(stop)));
        }
        Ok(None)
    }

    /// Gather input and prepare the context before a task request.
    async fn prepare_turn(
        &mut self,
        context: &mut Vec<Message>,
        result: &mut LoopResult,
        pending: &mut PendingInput,
    ) -> Result<TurnPreparation, Halt> {
        let config = self.config;
        // A turn exists to make a request: without budget, or once cancelled, none is made,
        // and queued input stays queued for the next run. The first turn of an attempt
        // starts with it, before the prompt, so this is checked before taking input as
        // well; a summary request may also have used the budget inside the current turn.
        if self.control.cancel.is_cancelled() {
            return Ok(TurnPreparation::Stop(LoopOutcome::Aborted));
        }

        if let Some(outcome) = exhausted(self.control, result) {
            return Ok(TurnPreparation::Stop(outcome));
        }

        // A new turn, unless one is in progress.
        self.start_turn(result).await;

        // Queued input is taken one message at a time, so a rejected message or a stopped
        // run leaves the rest of the queue for the next run.
        self.drain_queue(context, Queue::Steering).await?;
        if std::mem::take(&mut pending.take_follow_ups) {
            self.drain_queue(context, Queue::FollowUp).await?;
        }
        if let Some(message) = pending.requested.take() {
            self.input(context, message, None).await?;
        }
        if self.control.cancel.is_cancelled() {
            return Ok(TurnPreparation::Stop(LoopOutcome::Aborted));
        }
        if let Some(outcome) = exhausted(self.control, result) {
            return Ok(TurnPreparation::Stop(outcome));
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
            return Ok(TurnPreparation::Stop(LoopOutcome::Aborted));
        }
        // Summary requests count against the budget but are reported as a compaction,
        // not as turns of their own.
        let requests = self.prepare_context(context, result).await?;
        if self.control.cancel.is_cancelled() {
            return Ok(TurnPreparation::Stop(LoopOutcome::Aborted));
        }
        if let Some(outcome) = exhausted(self.control, result) {
            return Ok(TurnPreparation::Stop(outcome));
        }
        if requests > 0 {
            // Preparation is a request boundary too. Drain newly arrived steering
            // and record context hooks again before the next task request.
            return Ok(TurnPreparation::Retry);
        }
        Ok(TurnPreparation::Ready)
    }
}

/// The outcome when a budget leaves no room for another request: requests first, then tokens.
fn exhausted(control: &LoopControl<'_>, result: &LoopResult) -> Option<LoopOutcome> {
    if control
        .max_model_requests
        .is_some_and(|limit| result.model_requests >= limit)
    {
        return Some(LoopOutcome::ModelRequestLimit);
    }

    if control
        .token_budget
        .is_some_and(|budget| result.usage.input_and_output() >= budget)
    {
        return Some(LoopOutcome::TokenBudget);
    }

    None
}

fn completed(stop: StopReason) -> LoopOutcome {
    if stop == StopReason::Length {
        LoopOutcome::Truncated
    } else {
        LoopOutcome::Completed
    }
}

#[derive(Default)]
struct PendingInput {
    requested: Option<Message>,
    take_follow_ups: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ToolProgress {
    NoCalls,
    Continue,
    Terminated,
}

struct TurnResponse {
    message: AssistantMessage,
    turn: u32,
    tool_progress: ToolProgress,
}

enum ResponseHandling {
    Stop(LoopOutcome),
    Continue(Box<TurnResponse>),
}

enum TurnPreparation {
    Ready,
    Retry,
    Stop(LoopOutcome),
}
