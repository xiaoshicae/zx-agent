//! The agent loop: one [`Attempt`] runs turns over a session until the model stops, a budget
//! runs out or the run is cancelled. Extension behavior is supplied through bounded, ordered
//! hooks.
//!
//! The loop keeps no copy of the conversation. It records every step through its session
//! host before acting on it and reads the session's projection whenever it needs the context,
//! so what the model sees is always what the session holds.
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
use std::ops::ControlFlow;
use std::panic::AssertUnwindSafe;
use std::time::Instant;

use futures::future::join_all;
use futures::{FutureExt, StreamExt};
use tokio_util::sync::CancellationToken;

use super::command::SlashCommand;
use super::events::CustomMessageInfo;
use super::events::{AgentEvent, ApprovalResult, EventSink, EventTarget, RequestKind, RunTrace, ToolVerdict};
use super::hooks::{
    ContextEdit, HookCall, HookChain, HookContext, HookError, HookFailure, HookPoint, TurnDecision, TurnDisposition,
    TurnSummary, report_failure,
};
use super::policy::PolicyDecision;
use super::queue::{MessageQueues, Queue, QueueMode, QueuedMessage};
use super::report::RunOutcome;
use super::session_host::{Prepared, SessionHost, record_pending_entries};
use super::tool::{
    ActiveTools, Effects, ExecutionMode, MAX_DETAILS_BYTES, MAX_QUEUED_PROGRESS, Tool, ToolContext, ToolOutput,
};
use crate::agent::{Approval, ApprovalCache, ApprovalRequest, DEFAULT_APPROVAL_TIMEOUT, Interaction, NoHuman};
use crate::ai::{
    AssistantMessage, Message, ModelMeta, Provider, Reasoning, Request, StopReason, StreamEvent, SystemMessage,
    SystemPrompt, ToolCall, ToolResultMessage, Usage, UserMessageKind, duration_ms, now_ms,
};
use crate::session::SessionStore;

pub use config::{LoopConfig, LoopSpec};
use turn::{AfterTools, PendingInput, TurnResponse};

/// Why an attempt failed, with the response when the provider failed.
///
/// Accounting follows actual responses, so context edits cannot change usage or retries.
#[derive(Debug)]
pub enum AttemptFailure {
    /// A failed or cancelled response. Boxed so a successful result does not reserve space
    /// for a full response.
    Provider {
        message: Box<AssistantMessage>,
        /// The response's session entry.
        entry_id: String,
    },
    /// A hook or context preparation failed, or the session could not record.
    Halted(String),
}

/// What one attempt did. Only provider failures qualify for provider retries.
#[derive(Debug)]
pub struct AttemptResult {
    pub outcome: RunOutcome,
    /// Task turns started, numbered from the attempt's prior turns plus one.
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
    pub failure: Option<AttemptFailure>,
}

impl AttemptResult {
    /// Count the usage the tools reported with `results`.
    fn count_tool_usage(&mut self, results: &[ToolResultMessage]) {
        for result in results {
            if let Some(usage) = &result.usage {
                self.tool_usage.entry(result.tool_name.clone()).or_default().add(usage);
            }
        }
    }
}

/// One attempt of the loop over a session: prompts in, turns until the model stops, a
/// budget runs out or the attempt is cancelled. Each step is recorded in the session before
/// the loop acts on it, and the model is sent the session's projection.
///
/// Build one with [`Attempt::new`], set the budgets it needs, then [`Attempt::run`] it. The
/// Runner builds its attempts the same way, adding retries, limits and summaries around them.
pub struct Attempt<'a> {
    /// Model requests this attempt may make, summary requests included; `None` is
    /// unlimited, `Some(0)` makes no request.
    pub max_model_requests: Option<u32>,
    /// Tokens this attempt may use, input plus output as the provider reports them, summary
    /// requests included. Checked before each request, so the last one may go over; `None`
    /// is unlimited.
    pub token_budget: Option<u64>,
    /// What this attempt may spend, in the unit of the configured prices of the models it
    /// asks, summary requests included. Checked before each request like `token_budget`; a
    /// response without a price counts as nothing. `None` is unlimited.
    pub cost_budget: Option<f64>,
    /// Task turns of earlier attempts of the run, so that turn numbers continue.
    pub(crate) prior_task_turns: u32,
    /// Answer continuations scheduled in earlier attempts of the run.
    pub(crate) prior_answer_continuations: u32,
    /// 1 for the first attempt of a run, plus one for each retry.
    pub(crate) attempt_number: u32,
    config: &'a LoopConfig,
    /// Records each step before the loop acts on it, and holds the context.
    session: &'a mut dyn SessionHost,
    approvals: &'a mut ApprovalCache,
    sink: &'a dyn EventSink,
    trace: Trace<'a>,
    /// The number of the turn in progress, between its `turn_start` and `turn_end`.
    turn: Option<u32>,
    validators: HashMap<String, jsonschema::Validator>,
    /// Tool calls that a policy or an approval denied.
    denied_calls: u32,
}

/// The run an attempt belongs to: the Runner's, or one of the attempt's own. It stamps the
/// events, tells the hooks where they run, and carries the cancellation.
enum Trace<'a> {
    Run(&'a RunTrace),
    Own(RunTrace),
}

impl Trace<'_> {
    fn get(&self) -> &RunTrace {
        match self {
            Self::Run(trace) => trace,
            Self::Own(trace) => trace,
        }
    }

    fn context(&self) -> &HookContext {
        self.get().context()
    }

    fn cancel(&self) -> &CancellationToken {
        self.get().cancel()
    }
}

impl<'a> Attempt<'a> {
    /// An attempt of its own over `session`, cancelled by `cancel`, with no budget. Session
    /// grants go to `approvals`; events go to `sink`.
    pub fn new(
        config: &'a LoopConfig,
        session: &'a mut SessionStore,
        approvals: &'a mut ApprovalCache,
        cancel: &'a CancellationToken,
        sink: &'a dyn EventSink,
    ) -> Self {
        let trace = config.trace(Some(session.header().id.clone()), cancel.clone());
        Self::with(config, session, approvals, sink, Trace::Own(trace))
    }

    /// An attempt of the run `trace`, recording through `host`; the run's cancellation is
    /// the trace's.
    pub(crate) fn in_run(
        config: &'a LoopConfig,
        host: &'a mut dyn SessionHost,
        approvals: &'a mut ApprovalCache,
        sink: &'a dyn EventSink,
        trace: &'a RunTrace,
    ) -> Self {
        Self::with(config, host, approvals, sink, Trace::Run(trace))
    }

    fn with(
        config: &'a LoopConfig,
        session: &'a mut dyn SessionHost,
        approvals: &'a mut ApprovalCache,
        sink: &'a dyn EventSink,
        trace: Trace<'a>,
    ) -> Self {
        Self {
            max_model_requests: None,
            token_budget: None,
            cost_budget: None,
            prior_task_turns: 0,
            prior_answer_continuations: 0,
            attempt_number: 1,
            config,
            session,
            approvals,
            sink,
            trace,
            turn: None,
            validators: HashMap::new(),
            denied_calls: 0,
        }
    }

    /// Record `prompts` and run turns until the model stops, a budget runs out or the
    /// attempt is cancelled. Never panics on provider or tool failures; the result says how
    /// the attempt ended.
    pub async fn run(mut self, prompts: Vec<QueuedMessage>) -> AttemptResult {
        let mut result = AttemptResult {
            outcome: RunOutcome::Completed,
            task_turns: 0,
            model_requests: 0,
            answer_continuations: self.prior_answer_continuations,
            usage: Usage::default(),
            tool_usage: BTreeMap::new(),
            denied_calls: 0,
            failure: None,
        };
        let attempt = self.attempt_number;
        self.emit(AgentEvent::AgentStart { attempt }).await;

        match self.run_turns(prompts, &mut result).await {
            Ok(outcome) => result.outcome = outcome,
            Err(halt) => {
                let (outcome, error) = halt.into_outcome(self.trace.cancel());
                result.outcome = outcome;
                result.failure = Some(AttemptFailure::Halted(error));
            }
        }

        // A turn that stopped before its request finished still ends, so turn events pair up.
        self.end_turn().await;
        result.denied_calls = self.denied_calls;
        self.emit(AgentEvent::AgentEnd {
            outcome: result.outcome,
        })
        .await;
        result
    }

    /// Record the prompts, then run turns: prepare, request, handle the response, decide
    /// whether another turn follows.
    async fn run_turns(&mut self, prompts: Vec<QueuedMessage>, result: &mut AttemptResult) -> Result<RunOutcome, Halt> {
        self.initialize(prompts, result).await?;
        let mut pending = PendingInput::default();
        loop {
            let turn = match self.prepare_turn(result, &mut pending).await? {
                ControlFlow::Continue(turn) => turn,
                ControlFlow::Break(outcome) => return Ok(outcome),
            };
            let Some(message) = self.request_model(result, turn).await? else {
                return Ok(RunOutcome::Aborted);
            };
            let response = match self.handle_response(result, turn, message).await? {
                ControlFlow::Continue(response) => response,
                ControlFlow::Break(outcome) => return Ok(outcome),
            };
            if let ControlFlow::Break(outcome) = self.decide_next_turn(result, response, &mut pending).await? {
                return Ok(outcome);
            }
        }
    }

    /// Where this attempt's events go.
    fn target(&self) -> EventTarget<'_> {
        EventTarget {
            sink: self.sink,
            trace: self.trace.get(),
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
    async fn start_turn(&mut self, result: &mut AttemptResult) -> u32 {
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

    /// Validate the specification and record the system prompt's changes and the input.
    async fn initialize(&mut self, prompts: Vec<QueuedMessage>, result: &mut AttemptResult) -> Result<(), Halt> {
        let config = self.config;
        self.validators = config.spec.validators().map_err(Halt::Failed)?;
        self.start_turn(result).await;
        let changes = SystemPrompt::from_transcript(&self.session.messages()).diff(&config.spec.system_prompt);
        if !changes.is_empty() {
            self.append(Message::System(SystemMessage {
                sections: changes,
                timestamp: now_ms(),
            }))
            .await?;
        }
        // State an extension recorded before the run, such as from a command, precedes the
        // input it may refer to.
        self.record_pending_entries().await?;
        for message in prompts {
            self.queued_input(message, None).await?;
        }
        Ok(())
    }

    /// Gather input and prepare the context before a task request. Returns the turn that is
    /// ready for its request, or the outcome that ends the attempt first.
    async fn prepare_turn(
        &mut self,
        result: &mut AttemptResult,
        pending: &mut PendingInput,
    ) -> Result<ControlFlow<RunOutcome, u32>, Halt> {
        loop {
            // A turn exists to make a request: without budget, or once cancelled, none is
            // made, and queued input stays queued for the next run. Checked before taking
            // input as well, because a summary request can use the budget inside the turn.
            if let Some(outcome) = self.checkpoint(result) {
                return Ok(ControlFlow::Break(outcome));
            }
            let turn = self.start_turn(result).await;
            // State an extension asked to record is written before the input it may refer to.
            self.record_pending_entries().await?;
            // Queued input is taken one message at a time, so a rejected message or a stopped
            // run leaves the rest of the queue for the next run.
            self.drain_queue(Queue::Steering).await?;
            match std::mem::take(pending) {
                PendingInput::None => {}
                PendingInput::FollowUps => self.drain_queue(Queue::FollowUp).await?,
                PendingInput::Requested(message) => self.input(*message, None).await?,
            }
            if let Some(outcome) = self.checkpoint(result) {
                return Ok(ControlFlow::Break(outcome));
            }
            let edits = self
                .config
                .hooks
                .transform_context(&self.session.messages(), self.trace.context())
                .await
                .map_err(|error| Halt::Failed(self.report_hook_failure(error)))?;
            for edit in edits {
                self.edit(edit).await?;
            }
            if let Some(outcome) = self.checkpoint(result) {
                return Ok(ControlFlow::Break(outcome));
            }
            // Summary requests count against the budget but are reported as a compaction,
            // not as turns of their own.
            let requests = self.prepare_context(result).await?;
            if let Some(outcome) = self.checkpoint(result) {
                return Ok(ControlFlow::Break(outcome));
            }
            if requests == 0 {
                return Ok(ControlFlow::Continue(turn));
            }
            // Preparation is a request boundary too: take newly arrived steering and run the
            // context hooks again before the task request.
        }
    }

    /// Record the response of turn `turn`, execute its tool calls, and close the turn.
    /// Returns the response for the decision that follows, or the outcome that ends the
    /// attempt.
    async fn handle_response(
        &mut self,
        result: &mut AttemptResult,
        turn: u32,
        message: AssistantMessage,
    ) -> Result<ControlFlow<RunOutcome, TurnResponse>, Halt> {
        let stop = message.stop_reason;
        let calls = message.tool_calls();
        // The response is kept twice: in the session, and for the decision or the failure.
        let entry_id = self
            .record_finished_message(&Message::Assistant(message.clone()))
            .await?;
        if matches!(stop, StopReason::Error | StopReason::Aborted) {
            self.end_turn().await;
            result.failure = Some(AttemptFailure::Provider {
                message: Box::new(message),
                entry_id,
            });
            let outcome = if stop == StopReason::Aborted {
                RunOutcome::Aborted
            } else {
                RunOutcome::Error
            };
            return Ok(ControlFlow::Break(outcome));
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
        }
        self.end_turn().await;
        if self.trace.cancel().is_cancelled() {
            return Ok(ControlFlow::Break(RunOutcome::Aborted));
        }
        Ok(ControlFlow::Continue(TurnResponse {
            response: message,
            turn,
            after_tools,
        }))
    }

    /// The outcome that ends the attempt before the next request: cancellation, or a budget
    /// with no room for another request.
    fn checkpoint(&self, result: &AttemptResult) -> Option<RunOutcome> {
        if self.trace.cancel().is_cancelled() {
            return Some(RunOutcome::Aborted);
        }
        self.exhausted(result)
    }

    /// The outcome when a budget leaves no room for another request: requests first, then
    /// tokens, then cost.
    fn exhausted(&self, result: &AttemptResult) -> Option<RunOutcome> {
        if self
            .max_model_requests
            .is_some_and(|limit| result.model_requests >= limit)
        {
            return Some(RunOutcome::ModelRequestLimit);
        }
        if self
            .token_budget
            .is_some_and(|budget| result.usage.input_and_output() >= budget)
        {
            return Some(RunOutcome::TokenBudget);
        }
        if self
            .cost_budget
            .is_some_and(|budget| result.usage.total_cost.unwrap_or(0.0) >= budget)
        {
            return Some(RunOutcome::CostBudget);
        }
        None
    }
}

/// Why the loop stopped before the model finished.
enum Halt {
    /// A hook or context preparation failed; if the run was cancelled, it was aborted.
    Failed(String),
    /// The session could not record a step, so the loop must not act on it.
    Unrecorded(anyhow::Error),
}

impl Halt {
    /// How the attempt ended, and why. A failure after cancellation is an abort; a step the
    /// session could not record is an error even then, as the audit trail is incomplete.
    fn into_outcome(self, cancel: &CancellationToken) -> (RunOutcome, String) {
        match self {
            Self::Failed(error) if cancel.is_cancelled() => (RunOutcome::Aborted, error),
            Self::Failed(error) => (RunOutcome::Error, error),
            Self::Unrecorded(error) => (RunOutcome::Error, format!("{error:#}")),
        }
    }
}
