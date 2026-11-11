//! What follows a turn: the finish hooks' decision, the queued input that takes precedence
//! over it, and the outcome that ends the attempt instead.

use super::{AgentLoop, Halt};
use crate::agent::queue::Queue;
use crate::agent::{FinishedTurn, RunOutcome, TurnDecision, TurnDisposition};
use crate::ai::{AssistantMessage, Message, StopReason, UserMessageKind};

impl AgentLoop<'_> {
    /// Ask the finish hooks and decide what the next turn starts with. Ends the attempt when
    /// no turn follows.
    pub(super) async fn decide_next_turn(&mut self, response: TurnResponse) -> Result<PendingInput, Halt> {
        let TurnResponse {
            response,
            turn,
            after_tools,
        } = response;
        let needs_more = after_tools == AfterTools::Ran;
        let stop = response.stop_reason;
        let disposition = self.disposition(after_tools);
        let remaining = self.accounting.remaining_model_requests();
        let messages = self.host.session().messages();
        let summary = FinishedTurn {
            response: &response,
            messages: &messages,
            turn,
            answer_continuations: self.accounting.answer_continuations,
            remaining_model_requests: remaining,
            disposition,
        };
        let decision = self.finish_turn(&summary).await?;
        if decision == TurnDecision::Stop {
            return Err(Halt::Ended(completed(stop)));
        }
        // Inspect again: input may have arrived while the hook ran. Steering is taken at the
        // top of every turn; follow-ups only when the model has nothing left to do.
        let queues = &self.config.queues;
        if queues.has(Queue::Steering) {
            return Ok(PendingInput::None);
        }
        if !needs_more && queues.has(Queue::FollowUp) {
            return Ok(PendingInput::FollowUps);
        }
        let requested = requested_input(decision, disposition, remaining, stop)?;
        if requested.as_ref().is_some_and(continues_answer) {
            self.accounting.answer_continuations = self.accounting.answer_continuations.saturating_add(1);
        }
        match requested {
            Some(message) => Ok(PendingInput::Requested(Box::new(message))),
            None if needs_more => Ok(PendingInput::None),
            None => Err(Halt::Ended(completed(stop))),
        }
    }

    /// Ask the finish hooks what follows the turn `summary` describes.
    async fn finish_turn(&self, summary: &FinishedTurn<'_>) -> Result<TurnDecision, Halt> {
        self.config
            .hooks
            .finish_turn(summary, self.trace.context())
            .await
            .map_err(|error| Halt::Failed(self.report_hook_failure(error)))
    }

    /// What the normal loop would do after a response, before the finish hooks decide.
    fn disposition(&self, after_tools: AfterTools) -> TurnDisposition {
        let needs_more = after_tools == AfterTools::Ran;
        // Inspect queues without consuming them: stopping or failing in a hook must
        // leave accepted user input available to the next run.
        let queues = &self.config.queues;
        let queued = queues.has(Queue::Steering) || (!needs_more && queues.has(Queue::FollowUp));
        if needs_more || queued {
            TurnDisposition::Continue
        } else if after_tools == AfterTools::Terminated {
            TurnDisposition::Terminated
        } else {
            TurnDisposition::Stop
        }
    }
}

/// The input a finish decision asks the next turn to start with, once no queued input takes
/// precedence. Ends the attempt when the decision needs a request the budget does not leave.
fn requested_input(
    decision: TurnDecision,
    disposition: TurnDisposition,
    remaining_model_requests: Option<u32>,
    stop: StopReason,
) -> Result<Option<Message>, Halt> {
    match decision {
        TurnDecision::ContinueAnswer(text) if disposition == TurnDisposition::Stop => {
            if remaining_model_requests == Some(0) {
                return Err(Halt::Ended(if stop == StopReason::Length {
                    RunOutcome::Truncated
                } else {
                    RunOutcome::ModelRequestLimit
                }));
            }
            Ok(Some(Message::continuation(text)))
        }
        TurnDecision::FollowUp(text) if disposition != TurnDisposition::Continue => {
            if remaining_model_requests == Some(0) {
                return Err(Halt::Ended(RunOutcome::ModelRequestLimit));
            }
            Ok(Some(Message::user(text)))
        }
        TurnDecision::Stop | TurnDecision::Default | TurnDecision::ContinueAnswer(_) | TurnDecision::FollowUp(_) => {
            Ok(None)
        }
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

fn completed(stop: StopReason) -> RunOutcome {
    if stop == StopReason::Length {
        RunOutcome::Truncated
    } else {
        RunOutcome::Completed
    }
}

/// What the next turn starts with besides queued steering, decided at the end of the turn
/// before it.
#[derive(Default)]
pub(super) enum PendingInput {
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

/// A turn whose response and tool results are recorded.
pub(super) struct TurnResponse {
    pub(super) response: AssistantMessage,
    pub(super) turn: u32,
    pub(super) after_tools: AfterTools,
}
