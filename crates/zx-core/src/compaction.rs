//! Bounded, auditable compaction owned by the run, not the agent loop (pi:
//! coding-agent/src/core/compaction/compaction.ts).
//!
//! [`Config`] holds the budgets and [`Report`] says what one compaction cost and produced;
//! the `compactor` runs one compaction over a session, and `answer` bounds and parses the model's
//! answer. A host that makes the summaries itself gives the agent a [`Summarizer`].

mod answer;
mod compactor;

use std::time::Duration;

use anyhow::{Result, ensure};
use async_trait::async_trait;
use serde::Serialize;

use crate::ai::{Message, Usage};
use crate::guard::CancelSignal;
use crate::transcript::{CompactionSummary, ProjectedMessage, Transcript, without_details};

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

/// Makes the summary of a compaction in place of the kernel's own request to the model. An
/// agent takes one in [`AgentConfig::summarizer`](crate::AgentConfig::summarizer); without
/// one the kernel asks the model, and the request can be rebuilt from the session.
///
/// The kernel stays in charge either way: when a compaction runs, which records it reads (at
/// most 2 MiB of them), the records of its start and of its result or failure, the check that
/// every fact cites a record the compaction read, and the bounds of time and cancellation.
/// With a summarizer the start records its name instead of a request: how the summary was
/// made is not in the session.
#[async_trait]
pub trait Summarizer: Send + Sync {
    /// Names the summarizer in the start record of every compaction it serves. Not blank.
    fn name(&self) -> &str;

    /// The summary of `source`. Every fact cites the `entry_id` of at least one record of
    /// `source`; a summary holds at most 64 KiB and 128 statements of each kind, and must
    /// make the context smaller. An error is why the compaction fails, which its failure
    /// record keeps. The kernel waits at most [`Config::timeout`] and stops waiting when the
    /// run is cancelled; a panic fails the compaction.
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
    /// The run was cancelled before the compaction completed; the text says where.
    Cancelled(String),
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
    /// rejected a request as too long for its context. The result must then fit under each
    /// trigger that is set, so that the next request does not pass one again, and the events
    /// say `automatic`.
    Automatic,
}

/// Serialized transcript bytes as a model may see them (without tool result details), not a
/// tokenizer or a model context-window guarantee.
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
const BYTES_PER_TOKEN: usize = 4;

/// An estimate of the tokens the next request's context holds (pi:
/// `estimateProjectedContextTokens`): the prompt and output tokens counted by the last answer
/// whose usage still measures the context ([`Transcript::measured_context`]), plus a
/// quarter of the bytes of the messages after it; without such an answer, a quarter of all
/// bytes. Only an estimate: the usage counts the request as it was sent, and bytes are not
/// tokens.
pub fn context_tokens(session: &Transcript) -> Result<u64> {
    let (measured, rest) = session.measured_context();
    let known = measured.map_or(0, |usage| usage.input.saturating_add(usage.output));
    Ok(known.saturating_add(estimated_tokens(context_bytes(&rest)?)))
}

/// The tokens `bytes` of serialized context are estimated to hold when no answer measures
/// them, such as right after a compaction ([`context_tokens`]).
fn estimated_tokens(bytes: usize) -> u64 {
    bytes.div_ceil(BYTES_PER_TOKEN) as u64
}

/// Bytes of one message as a model may see it.
fn message_bytes(message: &Message) -> Result<usize> {
    serialized_bytes(&without_details(std::slice::from_ref(message))[0])
}
