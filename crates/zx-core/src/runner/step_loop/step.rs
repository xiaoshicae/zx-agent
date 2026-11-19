//! What follows a step: the finish hooks' decision, the queued input that takes precedence
//! over it, and the outcome that ends the attempt instead.

use super::{Halt, StepLoop};
use crate::ai::{AssistantMessage, Message, StopReason, UserMessageKind};
use crate::hooks::{FinishedStep, StepDecision, StepDisposition};
use crate::input::Queue;
use crate::runner::{Ending, Exhausted};

impl StepLoop<'_> {
    /// Ask the finish hooks and decide what the next step starts with. Ends the attempt when
    /// no step follows.
    pub(super) async fn decide_next_step(&mut self, response: StepResponse) -> Result<NextInput, Halt> {
        let StepResponse {
            response,
            step,
            after_tools,
        } = response;
        let needs_more = after_tools == AfterTools::Ran;
        let stop = response.stop_reason;
        let disposition = self.disposition(after_tools);
        let remaining = self.accounting.remaining_model_requests();
        let messages = self.scope.transcript.messages();
        let finished = FinishedStep {
            response: &response,
            messages: &messages,
            step,
            answer_continuations: self.accounting.answer_continuations,
            remaining_model_requests: remaining,
            disposition,
        };
        let decision = self.finish_step(&finished).await?;
        if decision == StepDecision::Stop {
            return Err(Halt::Ended(completed(stop)));
        }
        // Inspect again: input may have arrived while the hook ran. Steering is taken at the
        // top of every step; follow-ups only when the model has nothing left to do.
        let queues = &self.scope.config.queues;
        if queues.has(Queue::Steering) {
            return Ok(NextInput::None);
        }
        if !needs_more && queues.has(Queue::FollowUp) {
            return Ok(NextInput::FollowUps);
        }
        let requested = requested_input(decision, disposition, self.accounting.exhausted())?;
        if requested.as_ref().is_some_and(continues_answer) {
            self.accounting.answer_continuations = self.accounting.answer_continuations.saturating_add(1);
        }
        match requested {
            Some(message) => Ok(NextInput::Requested(Box::new(message))),
            None if needs_more => Ok(NextInput::None),
            None => Err(Halt::Ended(completed(stop))),
        }
    }

    /// Ask the finish hooks what follows the step `finished` describes.
    async fn finish_step(&self, finished: &FinishedStep<'_>) -> Result<StepDecision, Halt> {
        match self
            .scope
            .config
            .hooks
            .finish_step(finished, &self.scope.target.trace.hook_context())
            .await
        {
            Ok(decision) => Ok(decision),
            Err(error) => Err(self.hook_failed(error).await),
        }
    }

    /// What the normal loop would do after a response, before the finish hooks decide.
    fn disposition(&self, after_tools: AfterTools) -> StepDisposition {
        let needs_more = after_tools == AfterTools::Ran;
        // Inspect queues without consuming them: stopping or failing in a hook must
        // leave accepted user input available to the next run.
        let queues = &self.scope.config.queues;
        let queued = queues.has(Queue::Steering) || (!needs_more && queues.has(Queue::FollowUp));
        if needs_more || queued {
            StepDisposition::Continue
        } else if after_tools == AfterTools::Terminated {
            StepDisposition::Terminated
        } else {
            StepDisposition::Stop
        }
    }
}

/// The input a finish decision asks the next step to start with, once no queued input takes
/// precedence. Ends the attempt when the decision needs a request that the `exhausted`
/// budget does not leave; the run's outcome then says whether the answer was cut off.
fn requested_input(
    decision: StepDecision,
    disposition: StepDisposition,
    exhausted: Option<Exhausted>,
) -> Result<Option<Message>, Halt> {
    let message = match decision {
        StepDecision::ContinueAnswer(text) if disposition == StepDisposition::Stop => Message::continuation(text),
        StepDecision::FollowUp(text) if disposition != StepDisposition::Continue => Message::user(text),
        StepDecision::Stop | StepDecision::Default | StepDecision::ContinueAnswer(_) | StepDecision::FollowUp(_) => {
            return Ok(None);
        }
    };
    match exhausted {
        Some(exhausted) => Err(Halt::Ended(Ending::BudgetExhausted(exhausted))),
        None => Ok(Some(message)),
    }
}

/// Whether `message` continues the preceding answer rather than starting a new one.
fn continues_answer(message: &Message) -> bool {
    matches!(
        message,
        Message::User {
            kind: UserMessageKind::ContinueAnswer,
            ..
        }
    )
}

fn completed(stop: StopReason) -> Ending {
    if stop == StopReason::Length {
        Ending::Truncated
    } else {
        Ending::Completed
    }
}

/// What the next step starts with besides queued steering, decided at the end of the step
/// before it.
#[derive(Default)]
pub(super) enum NextInput {
    #[default]
    None,
    /// The follow-up queue: the model had nothing left to do.
    FollowUps,
    /// A message a finish hook requested: a continuation of the answer, or a follow-up of
    /// its own. Boxed: the variant is rare and the others carry nothing.
    Requested(Box<Message>),
}

/// What the tool calls of one response leave for the loop.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum AfterTools {
    /// The response called no tool.
    NoCalls,
    /// The calls ran; the model sees their results next.
    Ran,
    /// A tool asked the loop to end the attempt.
    Terminated,
}

/// A step whose response and tool results are recorded.
pub(super) struct StepResponse {
    pub(super) response: AssistantMessage,
    pub(super) step: u32,
    pub(super) after_tools: AfterTools,
}
