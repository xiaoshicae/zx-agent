//! What follows a turn: the finish hooks' decision, the queued input that takes precedence
//! over it, and the outcome that ends the attempt instead.

use super::{
    AssistantMessage, Engine, Halt, LoopOutcome, LoopResult, Message, Queue, StopReason, TurnDecision, TurnDisposition,
    TurnSummary, UserMessageKind,
};

impl Engine<'_, '_> {
    /// Ask the finish hook and decide whether another turn is needed.
    pub(super) async fn decide_next_turn(
        &mut self,
        context: &[Message],
        result: &mut LoopResult,
        response: TurnResponse,
        pending: &mut PendingInput,
    ) -> Result<Option<LoopOutcome>, Halt> {
        let TurnResponse {
            index,
            turn,
            tool_progress,
        } = response;
        let message = answer_at(context, index).expect(
            "the turn's response stays at the index it was pushed at: the context only grows until the next turn",
        );
        let needs_more = tool_progress == ToolProgress::Continue;
        let stop = message.stop_reason;
        let disposition = self.disposition(tool_progress);
        let remaining = self.remaining_model_requests(result);
        let summary = TurnSummary {
            response: message,
            messages: context,
            turn,
            answer_continuations: result.answer_continuations,
            remaining_model_requests: remaining,
            disposition,
        };
        let decision = self.finish_turn(&summary).await?;
        if decision == TurnDecision::Stop {
            return Ok(Some(completed(stop)));
        }
        // Inspect again: input may have arrived while the hook ran. It is taken at the top
        // of the next turn; follow-ups only when the model has nothing left to do.
        let queues = &self.config.queues;
        let steering = queues.has(Queue::Steering);
        let follow_ups = !needs_more && !steering && queues.has(Queue::FollowUp);
        if steering || follow_ups {
            *pending = if follow_ups {
                PendingInput::FollowUps
            } else {
                PendingInput::None
            };
            return Ok(None);
        }
        let requested = match requested_input(decision, disposition, remaining, stop) {
            Ok(requested) => requested,
            Err(outcome) => return Ok(Some(outcome)),
        };
        if requested.as_ref().is_some_and(continues_answer) {
            result.answer_continuations = result.answer_continuations.saturating_add(1);
        }
        *pending = match requested {
            Some(message) => PendingInput::Requested(Box::new(message)),
            None if needs_more => PendingInput::None,
            None => return Ok(Some(completed(stop))),
        };
        Ok(None)
    }

    /// Ask the finish hooks what follows the turn `summary` describes.
    async fn finish_turn(&self, summary: &TurnSummary<'_>) -> Result<TurnDecision, Halt> {
        self.config
            .hooks
            .finish_turn(summary, self.trace.context())
            .await
            .map_err(|error| Halt::Failed(self.report_hook_failure(error)))
    }

    /// Model requests the attempt may still make; `None` is unlimited.
    fn remaining_model_requests(&self, result: &LoopResult) -> Option<u32> {
        self.max_model_requests
            .map(|limit| limit.saturating_sub(result.model_requests))
    }

    /// What the normal loop would do after a response, before the finish hooks decide.
    fn disposition(&self, tool_progress: ToolProgress) -> TurnDisposition {
        let needs_more = tool_progress == ToolProgress::Continue;
        // Inspect queues without consuming them: stopping or failing in a hook must
        // leave accepted user input available to the next run.
        let queues = &self.config.queues;
        let queued = queues.has(Queue::Steering) || (!needs_more && queues.has(Queue::FollowUp));
        if needs_more || queued {
            TurnDisposition::Continue
        } else if tool_progress == ToolProgress::Terminated {
            TurnDisposition::Terminated
        } else {
            TurnDisposition::Stop
        }
    }
}

/// The input a finish decision asks the next turn to start with, once no queued input takes
/// precedence. An error is the outcome that ends the attempt because the decision needs a
/// request the budget does not leave.
fn requested_input(
    decision: TurnDecision,
    disposition: TurnDisposition,
    remaining_model_requests: Option<u32>,
    stop: StopReason,
) -> Result<Option<Message>, LoopOutcome> {
    match decision {
        TurnDecision::ContinueAnswer(text) if disposition == TurnDisposition::Stop => {
            if remaining_model_requests == Some(0) {
                return Err(if stop == StopReason::Length {
                    LoopOutcome::Truncated
                } else {
                    LoopOutcome::ModelRequestLimit
                });
            }
            Ok(Some(Message::continuation(text)))
        }
        TurnDecision::FollowUp(text) if disposition != TurnDisposition::Continue => {
            if remaining_model_requests == Some(0) {
                return Err(LoopOutcome::ModelRequestLimit);
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

/// The assistant message at `index` of `context`, if one is there.
fn answer_at(context: &[Message], index: usize) -> Option<&AssistantMessage> {
    match context.get(index)? {
        Message::Assistant(message) => Some(message),
        Message::System(_) | Message::User { .. } | Message::ToolResult(_) => None,
    }
}

fn completed(stop: StopReason) -> LoopOutcome {
    if stop == StopReason::Length {
        LoopOutcome::Truncated
    } else {
        LoopOutcome::Completed
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ToolProgress {
    NoCalls,
    Continue,
    Terminated,
}

/// A turn whose response and tool results are in the context.
#[derive(Clone, Copy)]
pub(super) struct TurnResponse {
    /// Where the response is in the context.
    pub(super) index: usize,
    pub(super) turn: u32,
    pub(super) tool_progress: ToolProgress,
}

pub(super) enum ResponseHandling {
    Stop(LoopOutcome),
    Continue(TurnResponse),
}

pub(super) enum TurnPreparation {
    /// The turn with this number is ready for its request.
    Ready(u32),
    Retry,
    Stop(LoopOutcome),
}
