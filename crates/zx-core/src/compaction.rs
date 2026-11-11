//! Bounded, auditable compaction owned by the session host, not the agent loop (pi:
//! coding-agent/src/core/compaction/compaction.ts).
//!
//! [`Config`] holds the budgets and [`Report`] says what one compaction cost and produced;
//! the `compactor` runs one compaction over a session, and `answer` bounds and parses the model's
//! answer.

mod answer;
mod compactor;

use std::time::Duration;

use anyhow::{Result, ensure};
use serde::Serialize;

use crate::ai::{Message, StopReason, Usage};
use crate::session::without_details;

pub(crate) use compactor::Compactor;

/// The most bytes of original records one compaction reads.
const MAX_SOURCE_BYTES: usize = 2 * 1024 * 1024;

/// Automatic trigger and summary request budgets. Default SDK configuration has no
/// automatic trigger; manual compaction still uses the remaining bounded settings.
#[derive(Clone, Debug)]
pub struct Config {
    /// Serialized transcript threshold; None disables the byte trigger of automatic
    /// compaction.
    pub trigger_bytes: Option<usize>,
    /// Estimated context tokens above which an automatic compaction runs, from the last
    /// response's reported usage and the bytes added since ([`context_tokens`]); None
    /// disables the token trigger. Either trigger runs a compaction.
    pub trigger_tokens: Option<u64>,
    /// Tokens kept free below a model's context window when the token trigger is derived
    /// from the window ([`Config::derive_trigger_tokens`]); pi's `reserveTokens`.
    pub reserve_tokens: u64,
    /// Recent message budget. The complete latest input and all system messages survive.
    pub keep_recent_bytes: usize,
    /// Output token budget for the summary request, independently of task requests.
    pub max_summary_tokens: u32,
    /// Bound for the whole summary stream, including a hung provider.
    pub timeout: Duration,
}

/// The default of [`Config::reserve_tokens`] (pi: the default `reserveTokens`).
pub const DEFAULT_RESERVE_TOKENS: u64 = 16_384;

impl Default for Config {
    fn default() -> Self {
        Self {
            trigger_bytes: None,
            trigger_tokens: None,
            reserve_tokens: DEFAULT_RESERVE_TOKENS,
            keep_recent_bytes: 64 * 1024,
            max_summary_tokens: 2048,
            timeout: Duration::from_secs(120),
        }
    }
}

impl Config {
    /// Whether automatic compaction is on: a byte or a token trigger is set.
    pub fn automatic(&self) -> bool {
        self.trigger_bytes.is_some() || self.trigger_tokens.is_some()
    }

    /// Set the token trigger from a model's context window, `reserve_tokens` below it (pi:
    /// `contextWindow - reserveTokens`), unless one is set already. A window no larger than
    /// the reserve leaves the trigger unset.
    pub fn derive_trigger_tokens(&mut self, context_window: u64) {
        let trigger = context_window.saturating_sub(self.reserve_tokens);
        if self.trigger_tokens.is_none() && trigger > 0 && Self::leaves_room(trigger, self.keep_recent_bytes) {
            self.trigger_tokens = Some(trigger);
        }
    }

    /// Whether a token trigger lies above the recent messages a summary keeps, as
    /// `triggerBytes` must: a summary could otherwise never bring the context under it.
    fn leaves_room(trigger_tokens: u64, keep_recent_bytes: usize) -> bool {
        trigger_tokens.saturating_mul(BYTES_PER_TOKEN as u64) > keep_recent_bytes as u64
    }

    /// Reject budgets that cannot make progress or cannot bound external waiting.
    pub fn validate(&self) -> Result<()> {
        ensure!(self.keep_recent_bytes > 0, "keepRecentBytes must be greater than zero");
        ensure!(
            self.trigger_tokens.is_none_or(|tokens| tokens > 0),
            "triggerTokens must be greater than zero (or absent to disable the token trigger)"
        );
        ensure!(
            self.trigger_tokens
                .is_none_or(|tokens| Self::leaves_room(tokens, self.keep_recent_bytes)),
            "triggerTokens must exceed a quarter of keepRecentBytes, the tokens the kept recent messages take"
        );
        ensure!(
            self.trigger_bytes.is_none_or(|bytes| bytes > self.keep_recent_bytes),
            "triggerBytes must exceed keepRecentBytes (or be zero to disable auto compaction)"
        );
        // The first summary reads about what precedes the kept recent messages.
        ensure!(
            self.trigger_bytes
                .is_none_or(|bytes| bytes - self.keep_recent_bytes <= MAX_SOURCE_BYTES),
            "triggerBytes may exceed keepRecentBytes by at most {MAX_SOURCE_BYTES} bytes, the most a summary reads"
        );
        ensure!(
            self.max_summary_tokens > 0 && self.max_summary_tokens <= 16_384,
            "maxSummaryTokens must be 1..=16384"
        );
        ensure!(!self.timeout.is_zero(), "summary timeout must be greater than zero");
        Ok(())
    }
}

/// Outcome of a standalone compact operation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Completed,
    #[default]
    Noop,
    Failed,
    Aborted,
}

/// Costs and audit record for one compaction, including unsuccessful requests.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub outcome: Outcome,
    pub requests: u32,
    pub usage: Usage,
    pub input_bytes: usize,
    pub output_bytes: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Report {
    fn failed(mut self, error: String) -> Self {
        self.outcome = Outcome::Failed;
        self.error = Some(error);
        self
    }

    fn aborted(mut self) -> Self {
        self.outcome = Outcome::Aborted;
        self.error = Some("compaction cancelled".into());
        self
    }
}

/// What one compaction did, and whether the session recorded all of it.
pub(crate) struct Compacted {
    pub report: Report,
    /// The session could not record a step of the compaction, so a run must stop (invariant 7).
    /// The report's `error` states the same.
    pub unrecorded: Option<anyhow::Error>,
}

impl Compacted {
    /// A compaction whose every step the session recorded, or that recorded none.
    fn recorded(report: Report) -> Self {
        Self {
            report,
            unrecorded: None,
        }
    }
}

/// Why a compaction runs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Trigger {
    Manual,
    /// The context exceeds `trigger_bytes` before a request.
    Automatic,
    /// The model rejected a request as too long for its context. Checked like an automatic
    /// compaction, whatever the context's size in bytes.
    Overflow,
}

impl Trigger {
    /// Whether the compaction runs on the host's initiative rather than on request, as the
    /// `automatic` field of its events says.
    fn is_automatic(self) -> bool {
        self != Self::Manual
    }
}

/// Serialized transcript bytes as a model may see them (without tool result details), not a
/// tokenizer or a model context-window guarantee.
pub fn context_bytes(messages: &[Message]) -> Result<usize> {
    Ok(serde_json::to_vec(&without_details(messages))?.len())
}

/// Bytes of serialized context that count as one token in an estimate (pi:
/// `estimateTokens`, `Math.ceil(chars / 4)`).
const BYTES_PER_TOKEN: usize = 4;

/// An estimate of the tokens the next request's context holds (pi: `estimateContextTokens`):
/// the prompt and output tokens the last answer with reported usage counted, plus a quarter
/// of the bytes of the messages after it; without such an answer, a quarter of all bytes.
/// Only an estimate: the usage counts the request as it was sent, and bytes are not tokens.
pub fn context_tokens(messages: &[Message]) -> Result<u64> {
    let counted = messages.iter().rposition(|message| {
        matches!(message, Message::Assistant(answer)
            if answer.usage.total_tokens > 0 && !matches!(answer.stop_reason, StopReason::Error | StopReason::Aborted))
    });
    let (known, rest) = match counted {
        Some(index) => {
            let Message::Assistant(answer) = &messages[index] else {
                unreachable!("rposition found an assistant message")
            };
            (
                answer.usage.input.saturating_add(answer.usage.output),
                &messages[index + 1..],
            )
        }
        None => (0, messages),
    };
    let bytes = context_bytes(rest)?;
    Ok(known.saturating_add(bytes.div_ceil(BYTES_PER_TOKEN) as u64))
}

/// Bytes of one message as a model may see it.
fn message_bytes(message: &Message) -> Result<usize> {
    Ok(serde_json::to_vec(&without_details(std::slice::from_ref(message))[0])?.len())
}
