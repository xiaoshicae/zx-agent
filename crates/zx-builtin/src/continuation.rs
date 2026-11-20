//! The hook that continues an answer cut off at the output token limit.

use async_trait::async_trait;
use zx_core::hooks::{FinishedStep, Hook, HookContext, HookResult, StepDecision, StepDisposition};

use crate::ai::StopReason;

/// What the hook asks the model by default when an answer was cut off; the kernel does not
/// depend on this wording.
pub const DEFAULT_PROMPT: &str = "Your previous response was cut off at the output token limit. Continue exactly \
where it stopped, without repeating earlier text, and keep the rest concise.";

/// The hook that asks the model to continue an answer cut off at the output token limit,
/// as long as the run has continued fewer answers than its limit. It keeps no state: the
/// kernel gives it the run's count of continuations with each finished step.
pub struct ContinueOnLength {
    max_continuations: u32,
    prompt: String,
}

impl ContinueOnLength {
    /// A hook that continues a cut-off answer at most `max_continuations` times per run, asking
    /// with [`DEFAULT_PROMPT`].
    pub fn new(max_continuations: u32) -> Self {
        Self {
            max_continuations,
            prompt: DEFAULT_PROMPT.to_string(),
        }
    }

    /// Ask the model to go on with `prompt` instead of [`DEFAULT_PROMPT`]; it is recorded as
    /// the user message of each continuation.
    pub fn with_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.prompt = prompt.into();
        self
    }
}

#[async_trait]
impl Hook for ContinueOnLength {
    async fn finish_step(&self, step: &FinishedStep<'_>, _ctx: &HookContext) -> HookResult<StepDecision> {
        // Whether a budget leaves room for the continuation is the run's to decide: one that
        // does not ends the run as out of budget, saying that the answer was cut off.
        if step.disposition == StepDisposition::Stop
            && step.response.stop_reason == StopReason::Length
            && step.answer_continuations < self.max_continuations
        {
            Ok(StepDecision::ContinueAnswer(self.prompt.clone()))
        } else {
            Ok(StepDecision::Default)
        }
    }
}
