//! The agent loop: an [`AgentLoop`] runs turns over a session until the model stops, a budget
//! runs out or the run is cancelled. A run runs it once per attempt: the first, and one more
//! for each retry or recovery from a context overflow. Extension behavior is supplied through
//! bounded, ordered hooks.
//!
//! The loop keeps no copy of the conversation. It records every step through its session
//! host before acting on it and reads the session's projection whenever it needs the context,
//! so what the model sees is always what the session holds.
//!
//! - `config`: what a loop runs with: [`LoopConfig`]
//! - `session_host`: what a loop needs from the session it runs in, which the run provides
//! - `context`: input, queued messages and context edits, recorded before the loop uses them
//! - `model`: recording a task request and streaming its response
//! - `tools`: checking, approving, running and recording the calls of a response
//! - `turn`: what follows a turn: the finish hooks' decision and the queued input

mod config;
mod context;
mod model;
mod session_host;
mod tools;
mod turn;

use std::collections::HashMap;

use tokio_util::sync::CancellationToken;

use super::accounting::RunAccounting;
use super::events::{AgentEvent, EventSink};
use super::hooks::HookError;
use super::interaction::ApprovalCache;
use super::queue::{Queue, QueuedMessage};
use super::report::RunOutcome;
use super::trace::{EventTarget, RunTrace};
use crate::ai::{AssistantMessage, Message, StopReason, SystemMessage, SystemPrompt, now_ms};

pub use config::LoopConfig;
pub(crate) use session_host::{PERSIST_FAILED, SessionHost, record_pending_entries};
use turn::{AfterTools, PendingInput, TurnResponse};

/// Why the loop stops. Every step of a turn returns it as its error, so `?` ends the attempt
/// wherever a step stops it, and [`AgentLoop::run`] returns the one that did. What the
/// attempt spent is in the run's accounting.
pub(crate) enum Halt {
    /// The attempt ends with this outcome: the model finished, a budget ran out, or the run
    /// was cancelled. Never [`RunOutcome::Error`]: a failure is one of the other variants,
    /// which say why.
    Ended(RunOutcome),
    /// The provider's response failed. Only this failure qualifies for a retry. The response
    /// is carried as it arrived: accounting follows actual responses, so context edits
    /// cannot change usage or retries. Boxed so that the other variants do not reserve space
    /// for a full response.
    ProviderFailed {
        message: Box<AssistantMessage>,
        /// The response's session entry.
        entry_id: String,
    },
    /// A hook or context preparation failed. When the run was cancelled, the attempt was
    /// aborted instead, and [`AgentLoop::run`] returns `Ended` with that outcome.
    Failed(String),
    /// The session could not record a step, so the loop must not act on it.
    Unrecorded(anyhow::Error),
}

impl Halt {
    /// The halt as the attempt reports it: a failure after cancellation is an abort; a step
    /// the session could not record is an error even then, as the audit trail is incomplete.
    fn aborted_if_cancelled(self, cancel: &CancellationToken) -> Self {
        match self {
            Self::Failed(_) if cancel.is_cancelled() => Self::Ended(RunOutcome::Aborted),
            Self::Ended(_) | Self::ProviderFailed { .. } | Self::Failed(_) | Self::Unrecorded(_) => self,
        }
    }

    /// The outcome of the attempt, as its `agent_end` event reports it.
    fn outcome(&self) -> RunOutcome {
        match self {
            Self::Ended(outcome) => *outcome,
            Self::ProviderFailed { .. } | Self::Failed(_) | Self::Unrecorded(_) => RunOutcome::Error,
        }
    }
}

/// The loop over a session in one attempt of a run: prompts in, turns until the model stops,
/// a budget runs out or the run is cancelled. Each step is recorded in the session before the
/// loop acts on it, and the model is sent the session's projection.
///
/// The Runner builds one per attempt with [`AgentLoop::new`] and [`AgentLoop::run`]s it;
/// retries, the time limit and recovery stay with the run.
pub(crate) struct AgentLoop<'a> {
    config: &'a LoopConfig,
    /// Lends the session that records each step before the loop acts on it, and holds the
    /// context.
    host: &'a mut dyn SessionHost,
    approvals: &'a mut ApprovalCache,
    /// What the run has spent and may spend: the loop counts into it as it goes and checks
    /// its budgets before each request, so the totals span the run's attempts.
    accounting: &'a mut RunAccounting,
    sink: &'a dyn EventSink,
    /// The run this attempt belongs to: it stamps the events, tells the hooks where they run,
    /// and carries the cancellation.
    trace: &'a RunTrace,
    /// The number of the turn in progress, between its `turn_start` and `turn_end`.
    turn: Option<u32>,
    validators: HashMap<String, jsonschema::Validator>,
}

impl<'a> AgentLoop<'a> {
    /// The loop for the attempt in progress of the run whose events go to `target`, recording
    /// through `host` and spending against `accounting`; the run's cancellation is its
    /// trace's. Session grants go to `approvals`.
    pub(crate) fn new(
        config: &'a LoopConfig,
        host: &'a mut dyn SessionHost,
        approvals: &'a mut ApprovalCache,
        accounting: &'a mut RunAccounting,
        target: EventTarget<'a>,
    ) -> Self {
        Self {
            config,
            host,
            approvals,
            accounting,
            sink: target.sink,
            trace: target.trace,
            turn: None,
            validators: HashMap::new(),
        }
    }

    /// Record `prompts` and run turns until the model stops, a budget runs out or the run
    /// is cancelled. Never panics on provider or tool failures; the halt says how the
    /// attempt ended.
    pub(crate) async fn run(mut self, prompts: Vec<QueuedMessage>) -> Halt {
        let attempt = self.accounting.attempts;
        self.emit(AgentEvent::AgentStart { attempt }).await;

        let halt = self.run_turns(prompts).await.aborted_if_cancelled(self.trace.cancel());

        // A turn that stopped before its request finished still ends, so turn events pair up.
        self.end_turn().await;
        self.emit(AgentEvent::AgentEnd {
            outcome: halt.outcome(),
        })
        .await;
        halt
    }

    /// Record the prompts, then run turns until a step stops the loop. Returns why it did.
    async fn run_turns(&mut self, prompts: Vec<QueuedMessage>) -> Halt {
        if let Err(halt) = self.initialize(prompts).await {
            return halt;
        }
        let mut pending = PendingInput::default();
        loop {
            match self.run_turn(pending).await {
                Ok(next) => pending = next,
                Err(halt) => return halt,
            }
        }
    }

    /// One turn: prepare it with `pending`, request the model, handle the response, and
    /// decide what the next turn starts with.
    async fn run_turn(&mut self, pending: PendingInput) -> Result<PendingInput, Halt> {
        let turn = self.prepare_turn(pending).await?;
        let message = self.request_model(turn).await?;
        let response = self.handle_response(turn, message).await?;
        self.decide_next_turn(response).await
    }

    /// Where this attempt's events go. The target borrows the run, not the loop, so that the
    /// loop can record through its host while it holds one.
    fn target(&self) -> EventTarget<'a> {
        EventTarget {
            sink: self.sink,
            trace: self.trace,
            hooks: &self.config.hooks,
        }
    }

    async fn emit(&self, event: AgentEvent) {
        self.target().emit(event).await;
    }

    /// Report a hook failure as a `hook_failed` event. Returns its description.
    fn report_hook_failure(&self, error: HookError) -> String {
        self.target().report_failure(error)
    }

    /// Start the next task turn unless one is in progress. Returns the number of the turn
    /// in progress.
    async fn start_turn(&mut self) -> u32 {
        if let Some(turn) = self.turn {
            return turn;
        }
        self.accounting.task_turns = self.accounting.task_turns.saturating_add(1);
        let turn = self.accounting.task_turns;
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
    async fn initialize(&mut self, prompts: Vec<QueuedMessage>) -> Result<(), Halt> {
        let config = self.config;
        self.validators = config.validators().map_err(Halt::Failed)?;
        self.start_turn().await;
        let changes = SystemPrompt::from_transcript(&self.host.session().messages()).diff(&config.system_prompt);
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

    /// Gather input, starting with `pending`, and prepare the context before a task request.
    /// Returns the turn that is ready for its request.
    async fn prepare_turn(&mut self, mut pending: PendingInput) -> Result<u32, Halt> {
        loop {
            // A turn exists to make a request: without budget, or once cancelled, none is
            // made, and queued input stays queued for the next run. Checked before taking
            // input as well, because a summary request can use the budget inside the turn.
            self.ensure_can_request()?;
            let turn = self.start_turn().await;
            // State an extension asked to record is written before the input it may refer to.
            self.record_pending_entries().await?;
            // Queued input is taken one message at a time, so a rejected message or a stopped
            // run leaves the rest of the queue for the next run.
            self.drain_queue(Queue::Steering).await?;
            match std::mem::take(&mut pending) {
                PendingInput::None => {}
                PendingInput::FollowUps => self.drain_queue(Queue::FollowUp).await?,
                PendingInput::Requested(message) => self.input(*message, None).await?,
            }
            self.ensure_can_request()?;
            let edits = self
                .config
                .hooks
                .transform_context(&self.host.session().messages(), self.trace.context())
                .await
                .map_err(|error| Halt::Failed(self.report_hook_failure(error)))?;
            for edit in edits {
                self.edit(edit).await?;
            }
            self.ensure_can_request()?;
            // Summary requests count against the budget but are reported as a compaction,
            // not as turns of their own.
            let requests = self.host.prepare_context(self.accounting).await?;
            self.ensure_can_request()?;
            if requests == 0 {
                return Ok(turn);
            }
            // Preparation is a request boundary too: take newly arrived steering and run the
            // context hooks again before the task request.
        }
    }

    /// Record the response of turn `turn`, execute its tool calls, and close the turn.
    /// Returns the response for the decision that follows.
    async fn handle_response(&mut self, turn: u32, message: AssistantMessage) -> Result<TurnResponse, Halt> {
        let stop = message.stop_reason;
        let calls = message.tool_calls();
        // The response is kept twice: in the session, and for the decision or the failure.
        let entry_id = self
            .record_finished_message(&Message::Assistant(message.clone()))
            .await?;
        if matches!(stop, StopReason::Error | StopReason::Aborted) {
            self.end_turn().await;
            return Err(if stop == StopReason::Aborted {
                Halt::Ended(RunOutcome::Aborted)
            } else {
                Halt::ProviderFailed {
                    message: Box::new(message),
                    entry_id,
                }
            });
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
            self.accounting.count_tool_usage(&batch.results);
        }
        self.end_turn().await;
        self.ensure_not_cancelled()?;
        Ok(TurnResponse {
            response: message,
            turn,
            after_tools,
        })
    }

    /// End the attempt unless another request may be made: the run is not cancelled and
    /// every budget has room for one.
    fn ensure_can_request(&self) -> Result<(), Halt> {
        self.ensure_not_cancelled()?;
        match self.accounting.exhausted() {
            Some(outcome) => Err(Halt::Ended(outcome)),
            None => Ok(()),
        }
    }

    /// End the attempt if the run was cancelled.
    fn ensure_not_cancelled(&self) -> Result<(), Halt> {
        if self.trace.cancel().is_cancelled() {
            return Err(Halt::Ended(RunOutcome::Aborted));
        }
        Ok(())
    }
}
