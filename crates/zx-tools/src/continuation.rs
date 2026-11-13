//! The hook that continues an answer cut off at the output token limit.

use async_trait::async_trait;

use crate::ai::StopReason;
use zx_core::{FinishedStep, Hook, HookContext, HookResult, StepDecision, StepDisposition};

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
impl Hook for ContinueOnLength {
    async fn finish_step(&self, step: &FinishedStep<'_>, _ctx: &HookContext) -> HookResult<StepDecision> {
        if step.disposition == StepDisposition::Stop
            && step.response.stop_reason == StopReason::Length
            && step.remaining_model_requests != Some(0)
            && step.answer_continuations < self.max_continuations
        {
            Ok(StepDecision::ContinueAnswer(CONTINUE_PROMPT.to_string()))
        } else {
            Ok(StepDecision::Default)
        }
    }
}
