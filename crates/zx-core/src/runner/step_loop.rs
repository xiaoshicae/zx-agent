//! The agent loop: a [`StepLoop`] runs steps over a session until the model stops, a budget
//! runs out or the run is cancelled. A run runs it once per attempt: the first, and one more
//! for each retry or recovery from a context overflow. Extension behavior is supplied through
//! bounded, ordered hooks.
//!
//! The loop keeps no copy of the conversation. It records every step in the session its
//! run lends it before acting on it, and reads the session's projection whenever it needs
//! the context, so what the model sees is always what the session holds.
//!
//! - `config`: what a loop runs with: [`LoopConfig`]
//! - `context`: input, queued messages and context edits, recorded before the loop uses them
//! - `model`: recording a task request and streaming its response
//! - `tools`: checking, approving, running and recording the calls of a response
//! - `step`: what follows a step: the finish hooks' decision and the queued input

mod config;
mod context;
mod model;
mod step;
mod tools;

use tokio_util::sync::CancellationToken;

use crate::ai::{AssistantMessage, Message, StopReason, SystemMessage, SystemPrompt, now_ms};
use crate::events::EventKind;
use crate::hooks::HookError;
use crate::input::{Input, Queue};
use crate::policy::ApprovalGate;
use crate::runner::final_answer::cut_off;
use crate::runner::run_host::RunHost;
use crate::runner::{Ending, EventTarget, RunAccounting, RunOutcome};

pub(crate) use config::LoopConfig;
pub(crate) use context::{PERSIST_FAILED, record_pending_entries};
use step::{AfterTools, NextInput, StepResponse};

/// Why the loop stops. Every part of a step returns it as its error, so `?` ends the attempt
/// wherever a step stops it, and [`StepLoop::run`] returns the one that did. What the
/// attempt spent is in the run's accounting.
pub(crate) enum Halt {
    /// The attempt ends without a failure: the model finished, a budget ran out, or the run
    /// was cancelled.
    Ended(Ending),
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
    /// aborted instead, and [`StepLoop::run`] returns `Ended` with that outcome.
    Failed(String),
    /// The session could not record a step, so the loop must not act on it.
    Unrecorded(anyhow::Error),
}

impl Halt {
    /// The halt as the attempt reports it: a failure after cancellation is an abort; a step
    /// the session could not record is an error even then, as the audit trail is incomplete.
    fn aborted_if_cancelled(self, cancel: &CancellationToken) -> Self {
        match self {
            Self::Failed(_) if cancel.is_cancelled() => Self::Ended(Ending::Cancelled),
            Self::Ended(_) | Self::ProviderFailed { .. } | Self::Failed(_) | Self::Unrecorded(_) => self,
        }
    }

    /// The outcome of the attempt, as its `attempt_end` event reports it; `cut_off` says
    /// whether the run's last answer was cut off at the output token limit.
    fn outcome(&self, cut_off: bool) -> RunOutcome {
        match self {
            Self::Ended(ending) => ending.outcome(cut_off),
            Self::ProviderFailed { .. } | Self::Failed(_) | Self::Unrecorded(_) => RunOutcome::Failed,
        }
    }
}

/// The loop over a session in one attempt of a run: prompts in, steps until the model stops,
/// a budget runs out or the run is cancelled. Each step is recorded in the session before the
/// loop acts on it, and the model is sent the session's projection.
///
/// The Runner builds one per attempt with [`StepLoop::new`] and [`StepLoop::run`]s it;
/// retries, the time limit and recovery stay with the run.
pub(crate) struct StepLoop<'a> {
    /// The run's borrow of its Runner, for this attempt: the session that records each step
    /// before the loop acts on it and holds the context, the configuration, where the run's
    /// events go and the run's trace, which carries the cancellation.
    host: RunHost<'a>,
    /// Where a call waits for the host's approval, and what the session was granted.
    approvals: &'a ApprovalGate,
    /// What the run has spent and may spend: the loop counts into it as it goes and checks
    /// its budgets before each request, so the totals span the run's attempts.
    accounting: &'a mut RunAccounting,
    /// The number of the step in progress, between its `step_start` and `step_end`.
    step: Option<u32>,
}

/// What an attempt starts with.
pub(crate) enum FirstInput {
    /// The prompt of a run, recorded before the first request.
    Prompt(Input),
    /// Nothing new: the attempt answers the conversation as it is, as a retry does.
    Conversation,
    /// The follow-ups that wait in the queue, taken as they are between steps: as many at
    /// a time as the queue says, and each only once the session has recorded it.
    QueuedFollowUps,
}

impl<'a> StepLoop<'a> {
    /// The loop for the attempt in progress of the run that lends it `host`, spending against
    /// `accounting`; the run's cancellation is its trace's. Calls wait for approval at
    /// `approvals`, which keeps the session's grants.
    pub(super) fn new(host: RunHost<'a>, approvals: &'a ApprovalGate, accounting: &'a mut RunAccounting) -> Self {
        Self {
            host,
            approvals,
            accounting,
            step: None,
        }
    }

    /// Take what the attempt starts with and run steps until the model stops, a budget runs
    /// out or the run is cancelled. Never panics on provider or tool failures; the halt says
    /// how the attempt ended.
    pub(crate) async fn run(mut self, first: FirstInput) -> Halt {
        let attempt = self.accounting.attempts;
        self.emit(EventKind::AttemptStart { attempt }).await;

        let halt = self
            .run_steps(first)
            .await
            .aborted_if_cancelled(self.host.target.trace.cancel());

        // A step that stopped before its request finished still ends, so step events pair up.
        self.end_step().await;
        let cut_off = cut_off(&self.host.session.messages_since(self.host.run_start));
        self.emit(EventKind::AttemptEnd {
            outcome: halt.outcome(cut_off),
        })
        .await;
        halt
    }

    /// Record what the attempt starts with, then run steps until a step stops the loop.
    /// Returns why it did.
    async fn run_steps(&mut self, first: FirstInput) -> Halt {
        let (prompt, mut pending) = match first {
            FirstInput::Prompt(prompt) => (Some(prompt), NextInput::None),
            FirstInput::Conversation => (None, NextInput::None),
            FirstInput::QueuedFollowUps => (None, NextInput::FollowUps),
        };
        if let Err(halt) = self.initialize(prompt).await {
            return halt;
        }
        loop {
            match self.run_step(pending).await {
                Ok(next) => pending = next,
                Err(halt) => return halt,
            }
        }
    }

    /// One step: prepare it with `pending`, request the model, handle the response, and
    /// decide what the next step starts with.
    async fn run_step(&mut self, pending: NextInput) -> Result<NextInput, Halt> {
        let step = self.prepare_step(pending).await?;
        let message = self.request_model(step).await?;
        let response = self.handle_response(step, message).await?;
        self.decide_next_step(response).await
    }

    /// Where this attempt's events go. The target borrows the run, not the loop, so that the
    /// loop can record in its run's session while it holds one.
    fn target(&self) -> EventTarget<'a> {
        self.host.target
    }

    async fn emit(&self, event: EventKind) {
        self.target().emit(event).await;
    }

    /// Report a hook failure as a `hook_failed` event. Returns its description.
    async fn report_hook_failure(&self, error: HookError) -> String {
        self.target().report_failure(error).await
    }

    /// A hook failure that ends the attempt, reported as a `hook_failed` event.
    async fn hook_failed(&self, error: HookError) -> Halt {
        Halt::Failed(self.report_hook_failure(error).await)
    }

    /// Start the next task step unless one is in progress. Returns the number of the step
    /// in progress.
    async fn start_step(&mut self) -> u32 {
        if let Some(step) = self.step {
            return step;
        }
        self.accounting.task_steps = self.accounting.task_steps.saturating_add(1);
        let step = self.accounting.task_steps;
        self.step = Some(step);
        self.emit(EventKind::StepStart { step }).await;
        step
    }

    /// End the step in progress, if there is one.
    async fn end_step(&mut self) {
        if let Some(step) = self.step.take() {
            self.emit(EventKind::StepEnd { step }).await;
        }
    }

    /// Record the system prompt's changes and the prompt, if there is one.
    async fn initialize(&mut self, prompt: Option<Input>) -> Result<(), Halt> {
        let config = self.host.config;
        self.start_step().await;
        let changes = SystemPrompt::from_transcript(&self.host.session.messages()).diff(&config.system_prompt);
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
        if let Some(message) = prompt {
            self.queued_input(message, None).await?;
        }
        Ok(())
    }

    /// Gather input, starting with `pending`, and prepare the context before a task request.
    /// Returns the step that is ready for its request.
    async fn prepare_step(&mut self, mut pending: NextInput) -> Result<u32, Halt> {
        loop {
            // A step exists to make a request: without budget, or once cancelled, none is
            // made, and queued input stays queued for the next run. Checked before taking
            // input as well, because a summary request can use the budget inside the step.
            self.ensure_can_request()?;
            let step = self.start_step().await;
            // State an extension asked to record is written before the input it may refer to.
            self.record_pending_entries().await?;
            // Queued input is taken one message at a time, so a rejected message or a stopped
            // run leaves the rest of the queue for the next run.
            self.drain_queue(Queue::Steering).await?;
            match std::mem::take(&mut pending) {
                NextInput::None => {}
                NextInput::FollowUps => self.drain_queue(Queue::FollowUp).await?,
                // A hook asked for it: nobody sent it.
                NextInput::Requested(message) => self.input(*message, None, None).await?,
            }
            self.ensure_can_request()?;
            let planned = self
                .host
                .config
                .hooks
                .transform_context(&self.host.session.messages(), &self.host.target.trace.context())
                .await;
            let edits = match planned {
                Ok(edits) => edits,
                Err(error) => return Err(self.hook_failed(error).await),
            };
            for edit in edits {
                self.edit(edit).await?;
            }
            self.ensure_can_request()?;
            // Summary requests count against the budget but are reported as a compaction,
            // not as steps of their own.
            let requests = self.host.prepare_context(self.accounting).await?;
            self.ensure_can_request()?;
            if requests == 0 {
                return Ok(step);
            }
            // Preparation is a request boundary too: take newly arrived steering and run the
            // context hooks again before the task request.
        }
    }

    /// Record the response of step `step`, execute its tool calls, and close the step.
    /// Returns the response for the decision that follows.
    async fn handle_response(&mut self, step: u32, message: AssistantMessage) -> Result<StepResponse, Halt> {
        let stop = message.stop_reason;
        let calls = message.tool_calls();
        // The response is kept twice: in the session, and for the decision or the failure.
        let entry_id = self
            .record_finished_message(&Message::Assistant(message.clone()))
            .await?;
        if matches!(stop, StopReason::Error | StopReason::Aborted) {
            self.end_step().await;
            return Err(if stop == StopReason::Aborted {
                Halt::Ended(Ending::Cancelled)
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
            after_tools = if batch.terminates() {
                AfterTools::Terminated
            } else {
                AfterTools::Ran
            };
        }
        self.end_step().await;
        self.ensure_not_cancelled()?;
        Ok(StepResponse {
            response: message,
            step,
            after_tools,
        })
    }

    /// End the attempt unless another request may be made: the run is not cancelled and
    /// every budget has room for one.
    fn ensure_can_request(&self) -> Result<(), Halt> {
        self.ensure_not_cancelled()?;
        match self.accounting.exhausted() {
            Some(budget) => Err(Halt::Ended(Ending::BudgetExhausted(budget))),
            None => Ok(()),
        }
    }

    /// End the attempt if the run was cancelled.
    fn ensure_not_cancelled(&self) -> Result<(), Halt> {
        if self.host.target.trace.cancel().is_cancelled() {
            return Err(Halt::Ended(Ending::Cancelled));
        }
        Ok(())
    }
}
