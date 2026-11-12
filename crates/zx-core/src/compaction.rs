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

use crate::ai::{Message, Usage};
use crate::session::{SessionStore, without_details};

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
    /// When the estimated tokens of the context ([`context_tokens`]) run an automatic
    /// compaction. Either trigger runs one.
    pub trigger_tokens: TokenTrigger,
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

/// The token trigger of automatic compaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenTrigger {
    /// No token trigger is set. A host that knows the model's context window may set one
    /// ([`Config::derive_trigger_tokens`]); until then there is none.
    Unset,
    /// No token trigger, whatever the context window.
    Off,
    /// Above this many estimated tokens.
    Above(u64),
}

/// The default of [`Config::reserve_tokens`] (pi: the default `reserveTokens`).
pub const DEFAULT_RESERVE_TOKENS: u64 = 16_384;

impl Default for Config {
    fn default() -> Self {
        Self {
            trigger_bytes: None,
            trigger_tokens: TokenTrigger::Unset,
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
        self.trigger_bytes.is_some() || matches!(self.trigger_tokens, TokenTrigger::Above(_))
    }

    /// Set the token trigger from a model's context window, `reserve_tokens` below it (pi:
    /// `contextWindow - reserveTokens`), unless one is set already or it is switched off. The
    /// trigger stays unset when the window is no larger than the reserve, or when the trigger
    /// would not lie above the recent messages a summary keeps.
    pub fn derive_trigger_tokens(&mut self, context_window: u64) {
        let trigger = context_window.saturating_sub(self.reserve_tokens);
        if self.trigger_tokens == TokenTrigger::Unset
            && trigger > 0
            && Self::leaves_room(trigger, self.keep_recent_bytes)
        {
            self.trigger_tokens = TokenTrigger::Above(trigger);
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
        if let TokenTrigger::Above(tokens) = self.trigger_tokens {
            ensure!(
                tokens > 0,
                "triggerTokens must be greater than zero (`TokenTrigger::Off` disables the token trigger)"
            );
            ensure!(
                Self::leaves_room(tokens, self.keep_recent_bytes),
                "triggerTokens must exceed a quarter of keepRecentBytes, the tokens the kept recent messages take"
            );
        }
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

/// How a compaction ended, as its [`Report`] states it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The summary is recorded and stands in for the records it read.
    Completed,
    /// There was nothing old enough to summarize.
    Noop,
    /// The compaction failed and left the context as it was; `error` says why.
    Failed,
    /// The compaction was cancelled and left the context as it was.
    Aborted,
}

/// What one compaction cost and how it ended, including a request that failed.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub outcome: Outcome,
    /// Model requests made: 0 or 1.
    pub requests: u32,
    /// What the summary request used; zero when none got an answer.
    pub usage: Usage,
    /// The context's size before the compaction.
    pub input_bytes: usize,
    /// The context's size after it; `input_bytes` unless the compaction completed.
    pub output_bytes: usize,
    /// The `compaction` record of a completed compaction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
    /// Why a compaction that did not complete ended as it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl From<Compacted> for Report {
    fn from(compacted: Compacted) -> Self {
        let Compacted {
            requests,
            usage,
            input_bytes,
            end,
        } = compacted;
        let (outcome, output_bytes, entry_id, error) = match end {
            End::Noop => (Outcome::Noop, input_bytes, None, None),
            End::Completed { entry_id, output_bytes } => (Outcome::Completed, output_bytes, Some(entry_id), None),
            End::Failed(error) => (Outcome::Failed, input_bytes, None, Some(error)),
            End::Aborted(error) => (Outcome::Aborted, input_bytes, None, Some(error)),
            End::Unrecorded(error) => (Outcome::Failed, input_bytes, None, Some(format!("{error:#}"))),
        };
        Self {
            outcome,
            requests,
            usage,
            input_bytes,
            output_bytes,
            entry_id,
            error,
        }
    }
}

/// What one compaction cost and how it ended, for the kernel; [`Report`] is the same for a
/// frontend.
pub(crate) struct Compacted {
    /// Model requests made: 0 or 1.
    pub(crate) requests: u32,
    /// What the summary request used; zero when none got an answer.
    pub(crate) usage: Usage,
    /// The context's size before the compaction.
    pub(crate) input_bytes: usize,
    pub(crate) end: End,
}

impl Compacted {
    /// A compaction that ended before it made a request.
    fn without_request(input_bytes: usize, end: End) -> Self {
        Self {
            requests: 0,
            usage: Usage::default(),
            input_bytes,
            end,
        }
    }
}

/// How a compaction ended.
pub(crate) enum End {
    /// There was nothing old enough to summarize while keeping the system messages and the
    /// latest input.
    Noop,
    /// The summary is recorded: the session's projection is the new context.
    Completed {
        /// The `compaction` record.
        entry_id: String,
        /// The context's size with the summary in place.
        output_bytes: usize,
    },
    /// The compaction failed, for this reason, and left the context as it was.
    Failed(String),
    /// The run was cancelled before the compaction completed; the text says where.
    Aborted(String),
    /// The session could not record a step of the compaction, so a run must stop
    /// (invariant 7).
    Unrecorded(anyhow::Error),
}

/// Why a compaction runs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Trigger {
    /// Someone asked for it.
    Manual,
    /// The run started it: the context passed a trigger before a request, or the model
    /// rejected a request as too long for its context. The result must then fit under the
    /// byte trigger, and the events say `automatic`.
    Automatic,
}

/// Serialized transcript bytes as a model may see them (without tool result details), not a
/// tokenizer or a model context-window guarantee.
pub fn context_bytes(messages: &[Message]) -> Result<usize> {
    Ok(serde_json::to_vec(&without_details(messages))?.len())
}

/// Bytes of serialized context that count as one token in an estimate (pi:
/// `estimateTokens`, `Math.ceil(chars / 4)`).
const BYTES_PER_TOKEN: usize = 4;

/// An estimate of the tokens the next request's context holds (pi:
/// `estimateProjectedContextTokens`): the prompt and output tokens counted by the last answer
/// whose usage still measures the context ([`SessionStore::measured_context`]), plus a
/// quarter of the bytes of the messages after it; without such an answer, a quarter of all
/// bytes. Only an estimate: the usage counts the request as it was sent, and bytes are not
/// tokens.
pub fn context_tokens(session: &SessionStore) -> Result<u64> {
    let (measured, rest) = session.measured_context();
    let known = measured.map_or(0, |usage| usage.input.saturating_add(usage.output));
    let bytes = context_bytes(&rest)?;
    Ok(known.saturating_add(bytes.div_ceil(BYTES_PER_TOKEN) as u64))
}

/// Bytes of one message as a model may see it.
fn message_bytes(message: &Message) -> Result<usize> {
    Ok(serde_json::to_vec(&without_details(std::slice::from_ref(message))[0])?.len())
}
