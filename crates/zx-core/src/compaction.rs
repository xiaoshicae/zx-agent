//! Bounded, auditable compaction owned by the run, not the agent loop (pi:
//! coding-agent/src/core/compaction/compaction.ts).
//!
//! [`Config`] holds the budgets and [`Report`] says what one compaction cost and produced;
//! the `compactor` runs one compaction over a session, and `answer` bounds and parses the model's
//! answer. A host that makes the summaries itself gives the agent a [`Summarizer`].

mod answer;
mod compactor;

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde::Serialize;

use crate::ai::{Message, ModelMeta, Usage};
use crate::guard::CancelSignal;
use crate::transcript::{CompactionSummary, ProjectedMessage, Transcript, without_details};

pub(crate) use self::compactor::Compactor;

/// The most bytes of original records one compaction reads.
pub const MAX_SOURCE_BYTES: usize = 2 * 1024 * 1024;

/// The most output tokens [`Config::max_summary_tokens`] may allow, which bounds what one
/// summary costs.
pub const MAX_SUMMARY_TOKENS: u32 = 16_384;

/// Automatic trigger and summary request budgets. Default SDK configuration has no
/// automatic trigger; manual compaction still uses the remaining bounded settings.
#[derive(Clone, Debug)]
pub struct Config {
    /// Bytes of the serialized context that start automatic compaction; `None` disables
    /// the byte trigger.
    pub trigger_bytes: Option<usize>,
    /// When the estimated tokens of the context ([`context_tokens`]) run an automatic
    /// compaction; `None` disables the token trigger. Either trigger runs one.
    pub trigger_tokens: Option<TokenTrigger>,
    /// Recent message budget. The complete latest input and all system messages survive.
    pub keep_recent_bytes: usize,
    /// Output token budget for the summary request, independently of task requests.
    pub max_summary_tokens: u32,
    /// Bound for the whole summary stream, including a hung provider.
    pub timeout: Duration,
}

/// The token trigger of automatic compaction. It is checked before each task request, for
/// the model the session asks at that time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenTrigger {
    /// Above this many estimated tokens, whatever the model.
    Above(u64),
    /// Above the context window of the model, less `reserve_tokens` (pi: `shouldCompact`,
    /// `contextWindow - reserveTokens`), with the window that
    /// [`AgentConfig::models`](crate::AgentConfig::models) gives that model, so that a
    /// session that switches model follows the new window. No token trigger for a model whose
    /// window is unknown or no larger than the reserve, or whose trigger would not lie above
    /// the recent messages a summary keeps.
    BelowWindow { reserve_tokens: u64 },
}

/// A reserve for [`TokenTrigger::BelowWindow`] (pi: the default `reserveTokens`).
pub const DEFAULT_RESERVE_TOKENS: u64 = 16_384;

impl Default for Config {
    fn default() -> Self {
        Self {
            trigger_bytes: None,
            trigger_tokens: None,
            keep_recent_bytes: 64 * 1024,
            max_summary_tokens: 2048,
            timeout: Duration::from_secs(120),
        }
    }
}

impl Config {
    /// Whether automatic compaction is on for requests to `model`: the byte trigger is set,
    /// or a token trigger applies to that model ([`Config::trigger_tokens_for`]).
    pub(crate) fn automatic(&self, models: &HashMap<String, ModelMeta>, model: &str) -> bool {
        self.trigger_bytes.is_some() || self.trigger_tokens_for(models, model).is_some()
    }

    /// The estimated tokens above which the context of a request to `model` runs an
    /// automatic compaction; `None` when no token trigger applies to that model. `models`
    /// gives the window that a [`TokenTrigger::BelowWindow`] trigger lies below.
    pub(crate) fn trigger_tokens_for(&self, models: &HashMap<String, ModelMeta>, model: &str) -> Option<u64> {
        match self.trigger_tokens? {
            TokenTrigger::Above(tokens) => Some(tokens),
            TokenTrigger::BelowWindow { reserve_tokens } => models
                .get(model)?
                .context_window?
                .checked_sub(reserve_tokens)
                .filter(|&trigger| Self::leaves_room(trigger, self.keep_recent_bytes)),
        }
    }

    /// Whether a token trigger lies above the recent messages a summary keeps, as
    /// `trigger_bytes` must: a summary could otherwise never bring the context under it.
    fn leaves_room(trigger_tokens: u64, keep_recent_bytes: usize) -> bool {
        // Tokens beyond `usize` take more bytes than any budget holds.
        usize::try_from(trigger_tokens).map_or(true, |tokens| {
            tokens.saturating_mul(BYTES_PER_TOKEN) > keep_recent_bytes
        })
    }

    /// Reject budgets that cannot make progress or cannot bound external waiting.
    pub fn validate(&self) -> Result<(), InvalidConfig> {
        if self.keep_recent_bytes == 0 {
            return Err(InvalidConfig::ZeroKeepRecentBytes);
        }
        if let Some(TokenTrigger::Above(tokens)) = self.trigger_tokens {
            if tokens == 0 {
                return Err(InvalidConfig::ZeroTriggerTokens);
            }
            if !Self::leaves_room(tokens, self.keep_recent_bytes) {
                return Err(InvalidConfig::TriggerTokensWithinKept);
            }
        }
        if let Some(bytes) = self.trigger_bytes {
            if bytes <= self.keep_recent_bytes {
                return Err(InvalidConfig::TriggerBytesWithinKept);
            }
            // The first summary reads about what precedes the kept recent messages.
            if bytes - self.keep_recent_bytes > MAX_SOURCE_BYTES {
                return Err(InvalidConfig::TriggerBytesBeyondSource);
            }
        }
        if !(1..=MAX_SUMMARY_TOKENS).contains(&self.max_summary_tokens) {
            return Err(InvalidConfig::MaxSummaryTokens);
        }
        if self.timeout.is_zero() {
            return Err(InvalidConfig::ZeroTimeout);
        }
        Ok(())
    }
}

/// A setting of a compaction [`Config`] that cannot work. `Display` names the field as the
/// struct does; a host that reads the settings from a file of its own matches the variant to
/// name its own key.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvalidConfig {
    /// `keep_recent_bytes` is zero.
    #[error("keep_recent_bytes must be greater than zero")]
    ZeroKeepRecentBytes,
    /// `trigger_tokens` is `Above(0)`.
    #[error("trigger_tokens must be greater than zero (`None` disables the token trigger)")]
    ZeroTriggerTokens,
    /// `trigger_tokens` lies within the recent messages a summary keeps, so a summary could
    /// never bring the context under it.
    #[error(
        "trigger_tokens must exceed keep_recent_bytes / {per_token}, the tokens the kept recent messages take",
        per_token = BYTES_PER_TOKEN
    )]
    TriggerTokensWithinKept,
    /// `trigger_bytes` does not exceed `keep_recent_bytes`.
    #[error("trigger_bytes must exceed keep_recent_bytes (`None` disables the byte trigger)")]
    TriggerBytesWithinKept,
    /// `trigger_bytes` exceeds `keep_recent_bytes` by more than [`MAX_SOURCE_BYTES`], the most
    /// a summary reads.
    #[error("trigger_bytes may exceed keep_recent_bytes by at most {max} bytes, the most a summary reads", max = MAX_SOURCE_BYTES)]
    TriggerBytesBeyondSource,
    /// `max_summary_tokens` is zero or over [`MAX_SUMMARY_TOKENS`].
    #[error("max_summary_tokens must be 1..={max}", max = MAX_SUMMARY_TOKENS)]
    MaxSummaryTokens,
    /// `timeout` is zero.
    #[error("summary timeout must be greater than zero")]
    ZeroTimeout,
}

/// Makes the summary of a compaction in place of the kernel's own request to the model. An
/// agent takes one in [`AgentConfig::summarizer`](crate::AgentConfig::summarizer); without
/// one the kernel asks the model, and the request can be rebuilt from the session.
///
/// The kernel stays in charge either way: when a compaction runs, which records it reads (at
/// most [`MAX_SOURCE_BYTES`] of them), the records of its start and of its result or
/// failure, the check that every fact cites a record the compaction read, and the bounds of
/// time and cancellation.
/// With a summarizer the start records its name instead of a request: how the summary was
/// made is not in the session.
#[async_trait]
pub trait Summarizer: Send + Sync {
    /// Names the summarizer in the start record of every compaction it serves. Not blank.
    fn name(&self) -> &str;

    /// The summary of `source`. Every fact cites the `entry_id` of at least one record of
    /// `source`; a summary holds at most [`MAX_SUMMARY_BYTES`] and [`MAX_SUMMARY_STATEMENTS`]
    /// statements of each kind, and must make the context smaller. An error is why the
    /// compaction fails, which its failure record keeps. The kernel waits at most
    /// [`Config::timeout`] and stops waiting when the run is cancelled; a panic fails the
    /// compaction.
    ///
    /// [`MAX_SUMMARY_BYTES`]: crate::transcript::MAX_SUMMARY_BYTES
    /// [`MAX_SUMMARY_STATEMENTS`]: crate::transcript::MAX_SUMMARY_STATEMENTS
    async fn summarize(&self, source: &SummarySource<'_>) -> Result<Summarized, String>;
}

/// What a summarizer reads. Fields are only ever added, so the struct is `#[non_exhaustive]`.
#[derive(Debug)]
#[non_exhaustive]
pub struct SummarySource<'a> {
    /// The records to summarize, oldest first, as a model may see them: without the details
    /// and usage of tool results.
    pub records: &'a [ProjectedMessage],
    /// The output tokens a summary may take ([`Config::max_summary_tokens`]), for a
    /// summarizer that asks a model.
    pub max_summary_tokens: u32,
    /// Cancelled when the run is, and when the kernel stopped waiting for the summary.
    pub cancel: CancelSignal,
}

/// What a summarizer made.
#[derive(Clone, Debug, PartialEq)]
pub struct Summarized {
    pub summary: CompactionSummary,
    /// What making the summary used. It is added to the run's usage, so it counts toward the
    /// token and cost budgets; zero for a summarizer that asks no model.
    pub usage: Usage,
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
    Cancelled,
}

/// What one compaction cost and how it ended, including a request that failed.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub outcome: Outcome,
    /// Summaries asked for: 0 or 1. Each counts as one model request of the run, whether the
    /// kernel asked the model or the host's summarizer made the summary.
    pub requests: u32,
    /// What making the summary used; zero when none was made.
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
            reply,
            usage,
            input_bytes,
            end,
        } = compacted;
        let requests = reply.requests();
        let (outcome, output_bytes, entry_id, error) = match end {
            End::Noop => (Outcome::Noop, input_bytes, None, None),
            End::Completed { entry_id, output_bytes } => (Outcome::Completed, output_bytes, Some(entry_id), None),
            End::Failed(error) => (Outcome::Failed, input_bytes, None, Some(error)),
            End::Cancelled(error) => (Outcome::Cancelled, input_bytes, None, Some(error)),
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
    /// What the summary request got back.
    pub(crate) reply: SummaryReply,
    /// What making the summary used; zero when none was made.
    pub(crate) usage: Usage,
    /// The context's size before the compaction.
    pub(crate) input_bytes: usize,
    pub(crate) end: End,
}

impl Compacted {
    /// A compaction that ended before it made a request.
    pub(crate) fn without_request(input_bytes: usize, end: End) -> Self {
        Self {
            reply: SummaryReply::NotRequested,
            usage: Usage::default(),
            input_bytes,
            end,
        }
    }
}

/// What the summary request of a compaction got back. A failure is paid for again only when
/// the request was answered: one that got no answer may succeed the next time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SummaryReply {
    /// No request was made.
    NotRequested,
    /// No complete answer: the provider failed, the stream broke, or it timed out.
    Missing,
    /// The model, or the host's summarizer, answered.
    Answered,
}

impl SummaryReply {
    /// The model requests it counts: 0 or 1.
    pub(crate) fn requests(self) -> u32 {
        match self {
            Self::NotRequested => 0,
            Self::Missing | Self::Answered => 1,
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
    /// The run was cancelled before the compaction completed: the text is `compaction
    /// cancelled`, or the failure that came with the cancellation.
    Cancelled(String),
    /// The session could not record a step of the compaction, so a run must stop: what it
    /// does next could not be recorded either.
    Unrecorded(anyhow::Error),
}

/// Whose initiative a compaction runs on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Initiative {
    /// Someone asked for it.
    Manual,
    /// The run started it: the context passed a trigger before a request, or the model
    /// rejected a request as too long for its context. The result must then fit under each
    /// trigger that is set, so that the next request does not pass one again, and the events
    /// say `automatic`.
    Automatic,
}

/// Bytes of `messages` serialized as a model may see them (without tool result details), not
/// a tokenizer or a model context-window guarantee.
pub fn context_bytes(messages: &[Message]) -> Result<usize> {
    serialized_bytes(&without_details(messages))
}

/// How many bytes `value` serializes to, counted without keeping them.
fn serialized_bytes(value: &impl serde::Serialize) -> Result<usize> {
    struct Count(usize);

    impl std::io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut count = Count(0);
    serde_json::to_writer(&mut count, value)?;
    Ok(count.0)
}

/// Bytes of serialized context that count as one token in an estimate (pi:
/// `estimateTokens`, `Math.ceil(chars / 4)`).
pub const BYTES_PER_TOKEN: usize = 4;

/// An estimate of the tokens the next request's context holds (pi:
/// `estimateProjectedContextTokens`): the prompt and output tokens counted by the last answer
/// whose usage still measures the context ([`Transcript::measured_context`]), plus an
/// estimate from the bytes of the messages after it; without such an answer, an estimate from
/// all bytes. Only an estimate: the usage counts the request as it was sent, and bytes are not
/// tokens.
pub fn context_tokens(transcript: &Transcript) -> Result<u64> {
    let (measured, rest) = transcript.measured_context();
    let known = measured.map_or(0, |usage| usage.input.saturating_add(usage.output));
    Ok(known.saturating_add(estimated_tokens(context_bytes(&rest)?)))
}

/// The tokens `bytes` of serialized context are estimated to hold when no answer measures
/// them, such as right after a compaction ([`context_tokens`]).
fn estimated_tokens(bytes: usize) -> u64 {
    // A `usize` fits in a `u64` on every target Rust supports; otherwise the count saturates.
    u64::try_from(bytes.div_ceil(BYTES_PER_TOKEN)).unwrap_or(u64::MAX)
}

/// Bytes of one message as a model may see it.
fn message_bytes(message: &Message) -> Result<usize> {
    serialized_bytes(&without_details(std::slice::from_ref(message))[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What is known about the model `id`: its context window of `tokens`, and no prices.
    fn window(id: &str, tokens: u64) -> HashMap<String, ModelMeta> {
        let meta = ModelMeta {
            context_window: Some(tokens),
            cost: None,
        };
        HashMap::from([(id.to_string(), meta)])
    }

    /// Settings whose token trigger lies `reserve_tokens` below the window of the model.
    fn below_window(reserve_tokens: u64) -> Config {
        Config {
            trigger_tokens: Some(TokenTrigger::BelowWindow { reserve_tokens }),
            ..Config::default()
        }
    }

    #[test]
    fn a_token_trigger_below_the_window_follows_the_window_of_the_model_asked() {
        let config = below_window(DEFAULT_RESERVE_TOKENS);
        let mut models = window("m", 100_000);
        models.extend(window("small", 50_000));

        assert_eq!(
            config.trigger_tokens_for(&models, "m"),
            Some(100_000 - DEFAULT_RESERVE_TOKENS)
        );
        assert_eq!(
            config.trigger_tokens_for(&models, "small"),
            Some(50_000 - DEFAULT_RESERVE_TOKENS)
        );
        assert_eq!(
            config.trigger_tokens_for(&models, "unknown"),
            None,
            "no window, no trigger"
        );
    }

    #[test]
    fn a_token_trigger_of_a_number_of_tokens_is_kept_whatever_the_window() {
        let config = Config {
            trigger_tokens: Some(TokenTrigger::Above(100_000 - DEFAULT_RESERVE_TOKENS)),
            ..Config::default()
        };

        assert_eq!(
            config.trigger_tokens_for(&window("m", 50_000), "m"),
            Some(100_000 - DEFAULT_RESERVE_TOKENS),
            "an explicit value stays"
        );
    }

    #[test]
    fn a_window_within_the_reserve_or_the_kept_messages_gives_no_token_trigger() {
        let small = below_window(1000);
        assert_eq!(
            small.trigger_tokens_for(&window("m", 1000), "m"),
            None,
            "a window within the reserve sets nothing"
        );
        assert!(
            !small.automatic(&window("m", 1000), "m"),
            "nothing turns automatic compaction on when the window is within the reserve"
        );
        // 20000 tokens less the default reserve would lie within the recent messages that a
        // summary keeps by default.
        assert_eq!(
            below_window(DEFAULT_RESERVE_TOKENS).trigger_tokens_for(&window("m", 20_000), "m"),
            None,
            "no trigger a summary could never bring the context under"
        );
    }

    #[test]
    fn a_token_trigger_must_lie_above_zero_and_the_kept_messages() {
        assert!(
            Config {
                trigger_tokens: Some(TokenTrigger::Above(100)),
                ..Config::default()
            }
            .validate()
            .unwrap_err()
            .to_string()
            .contains("keep_recent_bytes")
        );
        assert!(
            Config {
                trigger_tokens: Some(TokenTrigger::Above(0)),
                ..Config::default()
            }
            .validate()
            .is_err()
        );
        assert!(Config::default().validate().is_ok());
        assert!(!Config::default().automatic(&HashMap::new(), "m"));
    }
}
