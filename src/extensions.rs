//! Built-in policies implemented through the same Hooks interface as SDK extensions.

use async_trait::async_trait;

use crate::agent::ToolOutput;
use crate::agent::hooks::{HookResult, Hooks, TurnDecision, TurnDisposition, TurnSummary};
use crate::ai::{StopReason, ToolCall};

/// The unattended continuation text; the runtime does not depend on this wording.
pub const CONTINUE_PROMPT: &str = "Your previous response was cut off at the output token limit. Continue exactly where it \
stopped, without repeating earlier text, and keep the rest concise.";

/// Stateless continuation policy. The runtime supplies a per-run continuation count.
pub struct ContinueOnLength {
    max_continuations: u32,
}

impl ContinueOnLength {
    pub fn new(max_continuations: u32) -> Self {
        Self { max_continuations }
    }
}

#[async_trait]
impl Hooks for ContinueOnLength {
    async fn finish_turn(&self, turn: &TurnSummary<'_>) -> HookResult<TurnDecision> {
        if turn.disposition == TurnDisposition::Stop
            && turn.response.stop_reason == StopReason::Length
            && turn.remaining_turns != Some(0)
            && turn.continuations < self.max_continuations
        {
            Ok(TurnDecision::ContinueAnswer(CONTINUE_PROMPT.to_string()))
        } else {
            Ok(TurnDecision::Default)
        }
    }
}

/// Redacts exact occurrences of configured secrets from tool output before persistence.
///
/// Empty secrets are ignored. Longer matches run first so overlapping secrets do not
/// leave a suffix visible. This does not discover unknown credentials or filter inputs.
pub struct RedactSecrets {
    secrets: Vec<String>,
}

impl RedactSecrets {
    pub fn new(secrets: impl IntoIterator<Item = String>) -> Self {
        let mut secrets: Vec<_> = secrets.into_iter().filter(|secret| !secret.is_empty()).collect();
        secrets.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        secrets.dedup();
        Self { secrets }
    }
}

#[async_trait]
impl Hooks for RedactSecrets {
    async fn after_tool(&self, _call: &ToolCall, mut output: ToolOutput) -> HookResult<ToolOutput> {
        for secret in &self.secrets {
            output.content = output.content.replace(secret, "[REDACTED]");
        }
        Ok(output)
    }
}
