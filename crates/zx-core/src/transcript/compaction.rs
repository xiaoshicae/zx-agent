//! Compaction in the session: what a summary holds and the limits it keeps, the records
//! of a compaction's start, result and failure, and the rules a new fact follows.
//!
//! - `records`: appending those records, and the checks each passes when it is appended
//!   and when a session is loaded
//! - `source`: what a summary reads, and the request messages it was sent
//! - `tests`, compiled for tests only: the records of compactions, and what they do to the
//!   projection

mod records;
mod source;
#[cfg(test)]
mod tests;

use std::collections::HashSet;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub(crate) use self::source::{summary_input, summary_messages};
use crate::ai::{AssistantMessage, Message, Usage, UserMessageKind};

/// The largest summary, serialized as JSON, in bytes.
pub const MAX_SUMMARY_BYTES: usize = 64 * 1024;

/// The most facts, inferences or plans one summary holds.
pub const MAX_SUMMARY_STATEMENTS: usize = 128;

/// Every fact cites only records in `ids`.
fn cite_only(facts: &[SummaryFact], ids: &[String]) -> Result<()> {
    let sources: HashSet<&str> = ids.iter().map(String::as_str).collect();
    for fact in facts {
        for id in &fact.source_entry_ids {
            ensure!(
                sources.contains(id.as_str()),
                "summary reference {id:?} is not in the source messages"
            );
        }
    }
    Ok(())
}

/// A model-produced fact with references to source message entries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SummaryFact {
    pub text: String,
    pub source_entry_ids: Vec<String>,
}

/// A summary of the conversation: facts with the records they cite, inferences and plans.
///
/// Facts and model inference are deliberately separate. References are validated;
/// this does not establish that a model's assertion is entailed by its source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CompactionSummary {
    pub facts: Vec<SummaryFact>,
    pub inferences: Vec<String>,
    pub plans: Vec<String>,
}

impl CompactionSummary {
    /// A valid summary of the records `ids`: within the limits, and every fact cites `ids`.
    pub(crate) fn validate(&self, ids: &[String]) -> Result<()> {
        self.validate_limits()?;
        cite_only(&self.facts, ids)
    }

    /// The bounds every summary keeps: at most [`MAX_SUMMARY_BYTES`], and
    /// [`MAX_SUMMARY_STATEMENTS`] statements of each kind,
    /// nothing empty, and every fact cites at least one record.
    fn validate_limits(&self) -> Result<()> {
        ensure!(
            serde_json::to_vec(self)?.len() <= MAX_SUMMARY_BYTES,
            "summary exceeds {MAX_SUMMARY_BYTES} bytes"
        );
        ensure!(
            self.facts.len() <= MAX_SUMMARY_STATEMENTS
                && self.inferences.len() <= MAX_SUMMARY_STATEMENTS
                && self.plans.len() <= MAX_SUMMARY_STATEMENTS,
            "too many summary statements: {} facts, {} inferences, {} plans; at most {MAX_SUMMARY_STATEMENTS} of each",
            self.facts.len(),
            self.inferences.len(),
            self.plans.len()
        );
        ensure!(
            !self.facts.is_empty() || !self.inferences.is_empty() || !self.plans.is_empty(),
            "summary is empty"
        );
        for (index, fact) in self.facts.iter().enumerate() {
            ensure!(!fact.text.trim().is_empty(), "summary fact #{} is empty", index + 1);
            ensure!(
                !fact.source_entry_ids.is_empty(),
                "summary fact #{} has no sourceEntryIds",
                index + 1
            );
        }
        for (kind, statements) in [("inference", &self.inferences), ("plan", &self.plans)] {
            for (index, text) in statements.iter().enumerate() {
                ensure!(!text.trim().is_empty(), "summary {kind} #{} is empty", index + 1);
            }
        }
        Ok(())
    }

    /// The summary as the user message the model sees in place of the records it replaces.
    /// The text depends only on the summary, so a projection rebuilds the same message.
    pub(crate) fn message(&self, timestamp: u64) -> Message {
        let mut text = String::from("<context_summary>\nEnvironment facts (source entry IDs):\n");
        for fact in &self.facts {
            text.push_str(&format!("- {} [{}]\n", fact.text, fact.source_entry_ids.join(", ")));
        }
        text.push_str("Inferences (not independently verified):\n");
        for inference in &self.inferences {
            text.push_str(&format!("- {inference}\n"));
        }
        text.push_str("Plans:\n");
        for plan in &self.plans {
            text.push_str(&format!("- {plan}\n"));
        }
        text.push_str("</context_summary>");
        Message::User {
            content: text,
            images: Vec::new(),
            timestamp,
            kind: UserMessageKind::Summary,
        }
    }
}

/// What a summary request was sent with, besides the records it read: enough to rebuild the
/// request from the session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryRequest {
    pub provider: String,
    pub model: String,
    pub instructions: String,
    pub max_tokens: u32,
}

/// Who makes the summary of a compaction, as its start records it: in JSON either a
/// `request` object or a `summarizer` name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SummarizedBy {
    /// The kernel asks the model: what the request was sent with, enough to rebuild it from
    /// the session.
    Request(SummaryRequest),
    /// The host's summarizer, by its name. How it made the summary is not in the session.
    Summarizer(String),
}

/// The start of a compaction: what it reads and keeps, and who makes its summary.
///
/// Written and flushed before the summary is asked for. A compaction left unresolved after
/// cancellation or process failure has no effect on model context.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionStartedEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: u64,
    pub first_kept_entry_id: String,
    /// The latest input before the boundary, when the boundary falls among what followed
    /// that input. The summary reads it like every record before the boundary, and it is
    /// also kept in the context as it was sent, before the summary: work summarized in its
    /// middle keeps the request it answers. Absent when the boundary is at or before the
    /// latest input, which is then kept anyway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept_input_entry_id: Option<String>,
    /// The records the summary reads, in branch order.
    pub source_entry_ids: Vec<String>,
    #[serde(flatten)]
    pub by: SummarizedBy,
    pub input_bytes: usize,
}

/// A successful summary and the boundary it keeps from; the records it replaces in the
/// context stay in the session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: u64,
    pub compaction_id: String,
    pub first_kept_entry_id: String,
    pub summary: CompactionSummary,
    pub usage: Usage,
}

/// A failed or cancelled compaction, kept outside model context.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionFailedEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: u64,
    pub compaction_id: String,
    pub error: String,
    pub usage: Usage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<AssistantMessage>,
}

/// What the start of a compaction records, before its summary is asked for.
pub(crate) struct CompactionPlan {
    pub(crate) first_kept_entry_id: String,
    /// See [`CompactionStartedEntry::kept_input_entry_id`].
    pub(crate) kept_input_entry_id: Option<String>,
    pub(crate) source_entry_ids: Vec<String>,
    pub(crate) by: SummarizedBy,
    pub(crate) input_bytes: usize,
}
