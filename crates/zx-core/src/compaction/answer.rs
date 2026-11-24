//! The model's answer to a summary request: bounded as it streams and parsed into a
//! summary.

use std::collections::HashSet;

use anyhow::{Context, Result, bail, ensure};

use crate::ai::{AssistantMessage, ContentBlock, Delta, StopReason, ToolCall};
use crate::transcript::{CompactionSummary, MAX_SUMMARY_BYTES};

/// Reasoning a summary response may carry before its answer. Reasoning is not part of the
/// summary, so it does not count toward `MAX_SUMMARY_BYTES`; this separate, larger bound only
/// stops reasoning that does not end.
const MAX_SUMMARY_REASONING_BYTES: usize = 1024 * 1024;

/// The model's answer as a summary of the records `sources`, or why it is not one.
pub(super) fn parse_answer(message: &AssistantMessage, sources: &[String]) -> Result<CompactionSummary> {
    let mut summary = match message.stop_reason {
        StopReason::Stop if message.tool_calls().is_empty() => {
            serde_json::from_str::<CompactionSummary>(&message.text()).context("invalid summary JSON")?
        }
        StopReason::Stop => bail!("summary contains a tool call; tools are not permitted"),
        StopReason::Length => bail!("summary reached the output token limit"),
        StopReason::ToolUse => bail!("summary requested a tool call; tools are not permitted"),
        StopReason::Error | StopReason::Aborted => bail!("{}", message.failure_reason()),
    };
    drop_mistyped_references(&mut summary, sources);
    Ok(summary)
}

/// A fact that cites a record of `sources` keeps only its references to `sources`. A model
/// copies the IDs by hand and can mistype one beside the right one, which would otherwise
/// throw away the whole summary. A fact that cites none of them is left as it is, for the
/// check of the summary to reject: nothing shows where it came from.
fn drop_mistyped_references(summary: &mut CompactionSummary, sources: &[String]) {
    let sources: HashSet<&str> = sources.iter().map(String::as_str).collect();
    for fact in &mut summary.facts {
        if fact.source_entry_ids.iter().any(|id| sources.contains(id.as_str())) {
            fact.source_entry_ids.retain(|id| sources.contains(id.as_str()));
        }
    }
}

/// What a summary response holds, counted the way its limits are stated: the answer's text,
/// with any tool call or error message, toward `MAX_SUMMARY_BYTES`, and the reasoning toward
/// `MAX_SUMMARY_REASONING_BYTES`. The encoding that carries them is not counted: a server
/// that streams about a token per event would otherwise reach the limit with a fraction of
/// the text.
#[derive(Default)]
pub(super) struct ResponseBytes {
    answer: usize,
    reasoning: usize,
}

impl ResponseBytes {
    /// Count a streamed delta; fails once a limit is exceeded.
    pub(super) fn add(&mut self, delta: &Delta) -> Result<()> {
        match delta {
            Delta::Text { text } | Delta::ToolCallArgs { text, .. } => {
                self.answer = self.answer.saturating_add(text.len());
            }
            Delta::ToolCallStart { id, name } => {
                self.answer = self.answer.saturating_add(id.len()).saturating_add(name.len());
            }
            Delta::Thinking { text } => self.reasoning = self.reasoning.saturating_add(text.len()),
        }
        self.validate()
    }

    /// Count the content of a final message.
    pub(super) fn of(message: &AssistantMessage) -> Self {
        let mut bytes = Self::default();
        for block in &message.content {
            match block {
                ContentBlock::Text { text } => bytes.answer = bytes.answer.saturating_add(text.len()),
                ContentBlock::ToolCall(ToolCall {
                    id, name, arguments, ..
                }) => {
                    let call = id
                        .len()
                        .saturating_add(name.len())
                        .saturating_add(arguments.to_string().len());
                    bytes.answer = bytes.answer.saturating_add(call);
                }
                ContentBlock::Thinking { thinking, .. } => {
                    bytes.reasoning = bytes.reasoning.saturating_add(thinking.len());
                }
            }
        }
        // A failed response is recorded with its error message.
        if let Some(error) = &message.error_message {
            bytes.answer = bytes.answer.saturating_add(error.len());
        }
        bytes
    }

    /// Fails when a count is over its limit.
    pub(super) fn validate(&self) -> Result<()> {
        ensure!(
            self.answer <= MAX_SUMMARY_BYTES,
            "summary text exceeds {MAX_SUMMARY_BYTES} bytes"
        );
        ensure!(
            self.reasoning <= MAX_SUMMARY_REASONING_BYTES,
            "summary reasoning exceeds {MAX_SUMMARY_REASONING_BYTES} bytes"
        );
        Ok(())
    }
}
