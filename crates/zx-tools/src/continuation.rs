//! The hook that continues an answer cut off at the output token limit.

use async_trait::async_trait;

use crate::agent::{FinishedTurn, HookContext, HookResult, Hooks, TurnDecision, TurnDisposition};
use crate::ai::StopReason;

/// The unattended continuation text; the runtime does not depend on this wording.
const CONTINUE_PROMPT: &str = "Your previous response was cut off at the output token limit. Continue exactly where it \
stopped, without repeating earlier text, and keep the rest concise.";

/// Stateless continuation policy. The runtime supplies a per-run continuation count.
pub struct ContinueOnLength {
    max_continuations: u32,
}

impl ContinueOnLength {
    /// A hook that continues a cut-off answer at most `max_continuations` times per run.
    pub fn new(max_continuations: u32) -> Self {
        Self { max_continuations }
    }
}

#[async_trait]
impl Hooks for ContinueOnLength {
    async fn finish_turn(&self, turn: &FinishedTurn<'_>, _ctx: &HookContext) -> HookResult<TurnDecision> {
        if turn.disposition == TurnDisposition::Stop
            && turn.response.stop_reason == StopReason::Length
            && turn.remaining_model_requests != Some(0)
            && turn.answer_continuations < self.max_continuations
        {
            Ok(TurnDecision::ContinueAnswer(CONTINUE_PROMPT.to_string()))
        } else {
            Ok(TurnDecision::Default)
        }
    }
}
