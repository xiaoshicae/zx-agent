//! The agent loop. Extension behavior is supplied through bounded, ordered hooks.
//!
//! - `config`: what a loop runs with: [`LoopSpec`] and [`LoopConfig`]
//! - `context`: input, queued messages and context edits, recorded before the loop uses them
//! - `model`: recording a task request and streaming its response
//! - `tools`: checking, approving, running and recording the calls of a response
//! - `turn`: what follows a turn: the finish hooks' decision and the queued input

mod config;
mod context;
mod model;
mod tools;
mod turn;

use std::collections::{BTreeMap, HashMap};
use std::panic::AssertUnwindSafe;
use std::time::Instant;

use futures::future::join_all;
use futures::{FutureExt, StreamExt};
use tokio_util::sync::CancellationToken;

use super::command::SlashCommand;
use super::events::CustomMessageInfo;
use super::events::{
    AgentEvent, ApprovalResult, EventSink, EventTarget, LoopOutcome, RequestKind, RunTrace, ToolVerdict,
};
use super::hooks::{
    ContextEdit, HookCall, HookChain, HookError, HookFailure, HookPhase, TurnDecision, TurnDisposition, TurnSummary,
    report_failure,
};
use super::policy::Decision;
use super::queue::{MessageQueues, Queue, QueueMode, QueuedMessage};
use super::session_host::{NoSession, Prepared, SessionHost, record_pending_entries};
use super::tool::{
    ActiveTools, Effects, ExecutionMode, MAX_DETAILS_BYTES, MAX_QUEUED_PROGRESS, Tool, ToolContext, ToolOutput,
};
use crate::agent::{Approval, ApprovalCache, ApprovalRequest, DEFAULT_APPROVAL_TIMEOUT, Interaction, NoHuman};
use crate::ai::{
    AssistantMessage, Message, ModelMeta, Provider, Reasoning, Request, StopReason, StreamEvent, SystemMessage,
    SystemPrompt, ToolCall, ToolResultMessage, Usage, UserMessageKind, duration_ms, now_ms,
};

pub use config::{LoopConfig, LoopSpec};
use turn::{AfterTools, PendingInput, ResponseHandling, TurnPreparation, TurnResponse};

/// State borrowed from the run owner; grants survive retries without sharing a lock.
pub struct LoopControl<'a> {
    /// Model requests this attempt may make, summary requests included; `None` is unlimited.
    pub max_model_requests: Option<u32>,
    /// Tokens this attempt may use, input plus output as the provider reports them, summary
    /// requests included. Checked before each request, so the last one may go over; `None`
    /// is unlimited.
    pub token_budget: Option<u64>,
    /// What this attempt may spend, in the unit of the configured prices of the models it
    /// asks, summary
    /// requests included. Checked before each request like `token_budget`; a response
    /// without a price counts as nothing. `None` is unlimited.
    pub cost_budget: Option<f64>,
    /// Task turns of earlier attempts, so that turn numbers continue across retries.
    pub prior_task_turns: u32,
    /// Answer continuations scheduled in earlier attempts of this run.
    pub prior_answer_continuations: u32,
    pub approvals: &'a mut ApprovalCache,
    pub cancel: &'a CancellationToken,
    pub sink: &'a dyn EventSink,
    /// Records each step before the loop acts on it; the Runner supplies its session. Without
    /// one, the loop keeps messages and edits only in its context.
    pub(crate) session: Option<&'a mut dyn SessionHost>,
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
            cost_budget: None,
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

/// Why an attempt failed, with the response when the provider failed.
///
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
    /// What the tools spent on work of their own, by tool name: the `usage` of their
    /// results. Not part of `usage`, which the token budget counts.
    pub tool_usage: BTreeMap<String, Usage>,
    /// Tool calls that a policy or an approval denied in this attempt.
    pub denied_calls: u32,
    pub failure: Option<LoopFailure>,
}

impl LoopResult {
    /// Count the usage the tools reported with `results`.
    fn count_tool_usage(&mut self, results: &[ToolResultMessage]) {
        for result in results {
            if let Some(usage) = &result.usage {
                self.tool_usage.entry(result.tool_name.clone()).or_default().add(usage);
            }
        }
    }
}

/// Append prompts and run until the model stops, the budget expires, or cancellation.
/// Each step is recorded before the loop acts on it; the provider sees exactly this context.
/// A request budget of `None` is unlimited; `Some(0)` makes no model request.
pub async fn run_loop(
    context: &mut Vec<Message>,
    prompts: Vec<QueuedMessage>,
    config: &LoopConfig,
    control: &mut LoopControl<'_>,
) -> LoopResult {
    let mut result = LoopResult {
        outcome: LoopOutcome::Completed,
        task_turns: 0,
        model_requests: 0,
        answer_continuations: control.prior_answer_continuations,
        usage: Usage::default(),
        tool_usage: BTreeMap::new(),
        denied_calls: 0,
        failure: None,
    };

    // A run of its own that records nothing, unless the owner lends it a run and a session.
    let own_trace;
    let trace = match control.trace {
        Some(trace) => trace,
        None => {
            own_trace = config.trace(None, control.cancel.clone());
            &own_trace
        }
    };
    let mut no_session = NoSession;
    let session: &mut dyn SessionHost = match control.session.as_deref_mut() {
        Some(session) => session,
        None => &mut no_session,
    };
    let mut engine = Engine {
        config,
        max_model_requests: control.max_model_requests,
        token_budget: control.token_budget,
        cost_budget: control.cost_budget,
        prior_task_turns: control.prior_task_turns,
        attempt_number: control.attempt_number,
        approvals: &mut *control.approvals,
        cancel: control.cancel,
        sink: control.sink,
        session,
        trace,
        turn: None,
        validators: HashMap::new(),
        denied_calls: 0,
    };

    let attempt = engine.attempt_number;
    engine.emit(AgentEvent::AgentStart { attempt }).await;

    match engine.run(context, prompts, &mut result).await {
        Ok(outcome) => result.outcome = outcome,
        Err(halt) => {
            let (outcome, error) = halt.into_outcome(engine.cancel);
            result.outcome = outcome;
            result.failure = Some(LoopFailure::Host(error));
        }
    }

    // A turn that stopped before its request finished still ends, so turn events pair up.
    engine.end_turn().await;
    result.denied_calls = engine.denied_calls;

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

impl Halt {
    /// How the attempt ended, and why. A failure after cancellation is an abort; a step the
    /// session could not record is an error even then, as the audit trail is incomplete.
    fn into_outcome(self, cancel: &CancellationToken) -> (LoopOutcome, String) {
        match self {
            Self::Failed(error) if cancel.is_cancelled() => (LoopOutcome::Aborted, error),
            Self::Failed(error) | Self::Unrecorded(error) => (LoopOutcome::Error, error),
        }
    }
}

/// One attempt in progress: the configuration, what the run owner lent it, and its turn.
struct Engine<'a, 'b> {
    config: &'a LoopConfig,
    /// Model requests this attempt may make; `None` is unlimited.
    max_model_requests: Option<u32>,
    /// Tokens this attempt may use; `None` is unlimited.
    token_budget: Option<u64>,
    /// What this attempt may spend, in the unit of the prices; `None` is unlimited.
    cost_budget: Option<f64>,
    /// Task turns of earlier attempts, so that turn numbers continue across retries.
    prior_task_turns: u32,
    attempt_number: u32,
    approvals: &'b mut ApprovalCache,
    cancel: &'b CancellationToken,
    sink: &'b dyn EventSink,
    /// Records each step before the loop acts on it.
    session: &'b mut dyn SessionHost,
    trace: &'b RunTrace,
    /// The number of the turn in progress, between its `turn_start` and `turn_end`.
    turn: Option<u32>,
    validators: HashMap<String, jsonschema::Validator>,
    /// Tool calls that a policy or an approval denied.
    denied_calls: u32,
}

impl<'a, 'b> Engine<'a, 'b> {
    /// Where this attempt's events go. Borrows nothing of the engine, so a call can hold it
    /// while it records.
    fn target(&self) -> EventTarget<'b> {
        EventTarget {
            sink: self.sink,
            trace: self.trace,
        }
    }

    async fn emit(&self, event: AgentEvent) {
        self.config.hooks.emit(event, self.target()).await;
    }

    /// Report a hook failure as a `hook_failed` event. Returns its description.
    fn report_hook_failure(&self, error: HookError) -> String {
        report_failure(error, self.target())
    }

    /// Start the next task turn unless one is in progress. Returns the number of the turn
    /// in progress.
    async fn start_turn(&mut self, result: &mut LoopResult) -> u32 {
        if let Some(turn) = self.turn {
            return turn;
        }
        result.task_turns = result.task_turns.saturating_add(1);
        let turn = self.prior_task_turns.saturating_add(result.task_turns);
        self.turn = Some(turn);
        self.emit(AgentEvent::TurnStart { turn }).await;
        turn
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
        prompts: Vec<QueuedMessage>,
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
        // State an extension recorded before the run, such as from a command, precedes the
        // input it may refer to.
        self.record_pending_entries().await?;
        for message in prompts {
            self.queued_input(context, message, None).await?;
        }
        Ok(())
    }

    async fn run(
        &mut self,
        context: &mut Vec<Message>,
        prompts: Vec<QueuedMessage>,
        result: &mut LoopResult,
    ) -> Result<LoopOutcome, Halt> {
        self.initialize(context, prompts, result).await?;
        let mut pending = PendingInput::default();

        loop {
            let turn = match self.prepare_turn(context, result, &mut pending).await? {
                TurnPreparation::Ready(turn) => turn,
                TurnPreparation::Retry => continue,
                TurnPreparation::Stop(outcome) => return Ok(outcome),
            };
            let Some(message) = self.request_model(context, result, turn).await? else {
                return Ok(LoopOutcome::Aborted);
            };
            let response = match self.handle_response(context, result, turn, message).await? {
                ResponseHandling::Stop(outcome) => return Ok(outcome),
                ResponseHandling::Continue(response) => response,
            };
            if let Some(outcome) = self.decide_next_turn(context, result, response, &mut pending).await? {
                return Ok(outcome);
            }
        }
    }

    /// Record the response of turn `turn`, execute its tool calls, and close the turn.
    async fn handle_response(
        &mut self,
        context: &mut Vec<Message>,
        result: &mut LoopResult,
        turn: u32,
        message: AssistantMessage,
    ) -> Result<ResponseHandling, Halt> {
        let stop = message.stop_reason;
        let calls = message.tool_calls();
        // A failed response is kept twice: in the history and in the failure the run reports.
        let failed = matches!(stop, StopReason::Error | StopReason::Aborted).then(|| message.clone());
        let recorded = Message::Assistant(message);
        let entry_id = self.record_finished_message(&recorded).await?;
        let index = context.len();
        context.push(recorded);
        if let Some(message) = failed {
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

        let mut after_tools = AfterTools::NoCalls;
        if !calls.is_empty() {
            let batch = if stop == StopReason::Length {
                self.reject_truncated_tools(&calls).await?
            } else {
                self.execute_tools(&calls).await?
            };
            after_tools = if batch.terminate {
                AfterTools::Terminated
            } else {
                AfterTools::Ran
            };
            result.count_tool_usage(&batch.results);
            context.extend(batch.results.into_iter().map(Message::ToolResult));
        }
        self.end_turn().await;
        if self.cancel.is_cancelled() {
            return Ok(ResponseHandling::Stop(LoopOutcome::Aborted));
        }
        Ok(ResponseHandling::Continue(TurnResponse {
            index,
            turn,
            after_tools,
        }))
    }

    /// The outcome that ends the attempt before the next request: cancellation, or a budget
    /// with no room for another request.
    fn checkpoint(&self, result: &LoopResult) -> Option<LoopOutcome> {
        if self.cancel.is_cancelled() {
            return Some(LoopOutcome::Aborted);
        }
        self.exhausted(result)
    }

    /// The outcome when a budget leaves no room for another request: requests first, then
    /// tokens, then cost.
    fn exhausted(&self, result: &LoopResult) -> Option<LoopOutcome> {
        if self
            .max_model_requests
            .is_some_and(|limit| result.model_requests >= limit)
        {
            return Some(LoopOutcome::ModelRequestLimit);
        }
        if self
            .token_budget
            .is_some_and(|budget| result.usage.input_and_output() >= budget)
        {
            return Some(LoopOutcome::TokenBudget);
        }
        if self
            .cost_budget
            .is_some_and(|budget| result.usage.cost_usd.unwrap_or(0.0) >= budget)
        {
            return Some(LoopOutcome::CostBudget);
        }
        None
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
        // and queued input stays queued for the next run. Checked before taking input as
        // well, because a summary request can use the budget inside the turn.
        if let Some(outcome) = self.checkpoint(result) {
            return Ok(TurnPreparation::Stop(outcome));
        }
        let turn = self.start_turn(result).await;

        // State an extension asked to record is written before the input it may refer to.
        self.record_pending_entries().await?;
        // Queued input is taken one message at a time, so a rejected message or a stopped
        // run leaves the rest of the queue for the next run.
        self.drain_queue(context, Queue::Steering).await?;
        match std::mem::take(pending) {
            PendingInput::None => {}
            PendingInput::FollowUps => self.drain_queue(context, Queue::FollowUp).await?,
            PendingInput::Requested(message) => self.input(context, *message, None).await?,
        }
        if let Some(outcome) = self.checkpoint(result) {
            return Ok(TurnPreparation::Stop(outcome));
        }
        let edits = config
            .hooks
            .transform_context(context, self.trace.context())
            .await
            .map_err(|error| Halt::Failed(self.report_hook_failure(error)))?;
        for edit in edits {
            self.edit(context, edit).await?;
        }
        if let Some(outcome) = self.checkpoint(result) {
            return Ok(TurnPreparation::Stop(outcome));
        }
        // Summary requests count against the budget but are reported as a compaction,
        // not as turns of their own.
        let requests = self.prepare_context(context, result).await?;
        if let Some(outcome) = self.checkpoint(result) {
            return Ok(TurnPreparation::Stop(outcome));
        }
        if requests > 0 {
            // Preparation is a request boundary too: drain newly arrived steering and run
            // the context hooks again before the next task request.
            return Ok(TurnPreparation::Retry);
        }
        Ok(TurnPreparation::Ready(turn))
    }
}
