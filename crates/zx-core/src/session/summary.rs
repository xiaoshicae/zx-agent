//! Summaries in the session: what a summary holds and the limits it keeps, the records of
//! its attempt, result and failure, and the rules a new fact follows.
//!
//! - `records`: appending those records, and the checks each passes when it is appended
//!   and when a session is loaded
//! - `source`: what a summary reads, and the request messages it was sent

mod records;
mod source;

use std::collections::HashSet;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::ai::{AssistantMessage, Message, Usage, UserMessageKind};

pub(crate) use source::summary_messages;

/// The largest serialized summary, carried facts included.
pub(crate) const MAX_SUMMARY_BYTES: usize = 64 * 1024;

/// The most facts, inferences or plans one summary holds.
pub(crate) const MAX_STATEMENTS: usize = 128;

/// Every fact cites only records in `ids`.
pub(crate) fn cite_only(facts: &[SummaryFact], ids: &[String]) -> Result<()> {
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

/// How a summary reads the history before its boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryMode {
    /// Every summary reads all original records before its boundary again, never an earlier
    /// summary. A summary whose sources are corrected is invalid as a whole. Reading is
    /// bounded, so a long session eventually cannot be summarized this way.
    #[default]
    Full,
    /// A summary after an earlier one carries that one's facts over unchanged and reads only
    /// the original records after its boundary, plus earlier records corrected since. A
    /// correction invalidates only the facts that cite the corrected record.
    Incremental,
}

impl SummaryMode {
    fn is_full(&self) -> bool {
        *self == Self::Full
    }
}

/// A model-produced fact with references to source message entries. In an incremental
/// summary every fact has an `id`, `<attempt that created it>-<number>`, and a new fact may
/// name the earlier facts it `supersedes`; full summaries have neither.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SummaryFact {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub text: String,
    pub source_entry_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supersedes: Vec<String>,
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
    /// A full summary: within the limits, without fact IDs, and every fact cites `ids`.
    pub(crate) fn validate(&self, ids: &[String]) -> Result<()> {
        self.validate_limits()?;
        ensure!(
            self.facts
                .iter()
                .all(|fact| fact.id.is_none() && fact.supersedes.is_empty()),
            "a full summary has no fact IDs or supersedes"
        );
        cite_only(&self.facts, ids)
    }

    /// The bounds every summary keeps, carried facts included: at most 64 KiB and 128
    /// statements of each kind, nothing empty, and every fact cites at least one record.
    pub(crate) fn validate_limits(&self) -> Result<()> {
        ensure!(
            serde_json::to_vec(self)?.len() <= MAX_SUMMARY_BYTES,
            "summary exceeds {MAX_SUMMARY_BYTES} bytes"
        );
        ensure!(
            self.facts.len() <= MAX_STATEMENTS
                && self.inferences.len() <= MAX_STATEMENTS
                && self.plans.len() <= MAX_STATEMENTS,
            "too many summary statements: {} facts, {} inferences, {} plans; at most {MAX_STATEMENTS} of each",
            self.facts.len(),
            self.inferences.len(),
            self.plans.len()
        );
        ensure!(
            !self.facts.is_empty() || !self.inferences.is_empty() || !self.plans.is_empty(),
            "summary is empty"
        );
        for (index, fact) in self.facts.iter().enumerate() {
            ensure!(
                !fact.text.trim().is_empty(),
                "summary fact {} is empty",
                fact_label(index, fact)
            );
            ensure!(
                !fact.source_entry_ids.is_empty(),
                "summary fact {} has no sourceEntryIds",
                fact_label(index, fact)
            );
        }
        for (kind, statements) in [("inference", &self.inferences), ("plan", &self.plans)] {
            for (index, text) in statements.iter().enumerate() {
                ensure!(!text.trim().is_empty(), "summary {kind} #{} is empty", index + 1);
            }
        }
        Ok(())
    }

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
            timestamp,
            kind: UserMessageKind::Summary,
        }
    }
}

/// Information needed to reconstruct a summarization request with its source IDs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SummaryRequest {
    pub provider: String,
    pub model: String,
    pub instructions: String,
    pub max_tokens: u32,
}

/// How an error names a fact: its ID, or its position from 1 when it has none.
fn fact_label(index: usize, fact: &SummaryFact) -> String {
    match &fact.id {
        Some(id) => id.clone(),
        None => format!("#{}", index + 1),
    }
}

/// The earlier facts that `new` supersedes, each once, in the order named. An error names a
/// fact that is not among `previous`.
pub(crate) fn superseded_by(previous: &[SummaryFact], new: &[SummaryFact]) -> Result<Vec<String>> {
    let mut superseded: Vec<String> = Vec::new();
    for id in new.iter().flat_map(|fact| &fact.supersedes) {
        ensure!(
            previous.iter().any(|fact| fact.id.as_ref() == Some(id)),
            "a new fact supersedes {id:?}, which is not an earlier fact that still holds"
        );
        if !superseded.contains(id) {
            superseded.push(id.clone());
        }
    }
    Ok(superseded)
}

/// A summary attempt: what it reads and keeps, and the request it makes.
///
/// Written and flushed before the summary model is called. An unresolved attempt
/// after cancellation or process failure has no effect on model context.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionStartedEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: u64,
    pub first_kept_entry_id: String,
    /// The records the summary reads, in branch order. An incremental summary that continues
    /// an earlier one reads the corrected earlier records first, then those after its boundary.
    pub source_entry_ids: Vec<String>,
    pub request: SummaryRequest,
    pub input_bytes: usize,
    /// Written only for incremental summaries (format v5).
    #[serde(default, skip_serializing_if = "SummaryMode::is_full")]
    pub mode: SummaryMode,
    /// The summary an incremental one continues: its facts are carried over.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continues: Option<String>,
}

/// A successful summary and retained boundary; raw records are never removed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: u64,
    pub attempt_id: String,
    pub first_kept_entry_id: String,
    /// For an incremental summary: the carried facts that still hold, then the new ones.
    pub summary: CompactionSummary,
    pub usage: Usage,
    /// IDs of earlier facts that a new fact superseded; they stay in earlier records only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub superseded: Vec<String>,
    /// IDs of the oldest earlier facts left out to stay within the limits.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evicted: Vec<String>,
}

/// A failed or cancelled summary attempt, kept outside model context.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionFailedEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: u64,
    pub attempt_id: String,
    pub error: String,
    pub usage: Usage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<AssistantMessage>,
}

/// Inputs recorded before starting a summary request.
pub struct CompactionPlan {
    pub first_kept_entry_id: String,
    pub source_entry_ids: Vec<String>,
    pub request: SummaryRequest,
    pub input_bytes: usize,
    pub mode: SummaryMode,
    /// The summary an incremental one continues; `None` otherwise.
    pub continues: Option<String>,
}

/// The result of an incremental summary: the merged summary, and the earlier facts it no
/// longer carries.
pub struct MergedSummary {
    pub summary: CompactionSummary,
    pub superseded: Vec<String>,
    pub evicted: Vec<String>,
}
