//! Versioned compaction records and validation; all mutations append to the tree.

use std::collections::HashSet;

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::ai::{AssistantMessage, Message, Usage, UserMessageKind, now_ms};

use super::{Entry, ProjectedMessage, SESSION_VERSION, SessionStore};

/// A model-produced fact with references to source message entries.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SummaryFact {
    pub text: String,
    pub source_entry_ids: Vec<String>,
}

/// Facts and model inference are deliberately separate. References are validated;
/// this does not establish that a model's assertion is entailed by its source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CompactionSummary {
    pub facts: Vec<SummaryFact>,
    pub inferences: Vec<String>,
    pub plans: Vec<String>,
}

impl CompactionSummary {
    pub(crate) fn validate(&self, ids: &[String]) -> Result<()> {
        ensure!(
            serde_json::to_vec(self)?.len() <= 64 * 1024,
            "summary exceeds 65536 bytes"
        );
        ensure!(
            self.facts.len() <= 128 && self.inferences.len() <= 128 && self.plans.len() <= 128,
            "too many summary statements"
        );
        ensure!(
            !self.facts.is_empty() || !self.inferences.is_empty() || !self.plans.is_empty(),
            "summary is empty"
        );
        let sources: HashSet<&str> = ids.iter().map(String::as_str).collect();
        for fact in &self.facts {
            ensure!(!fact.text.trim().is_empty(), "summary fact is empty");
            ensure!(!fact.source_entry_ids.is_empty(), "summary fact has no sourceEntryIds");
            for id in &fact.source_entry_ids {
                ensure!(
                    sources.contains(id.as_str()),
                    "summary reference {id:?} is not in the source messages"
                );
            }
        }
        for text in self.inferences.iter().chain(&self.plans) {
            ensure!(!text.trim().is_empty(), "summary statement is empty");
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
            kind: UserMessageKind::Prompt,
        }
    }
}

/// Information needed to reconstruct a summarization request with its source IDs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SummaryRequest {
    pub provider: String,
    pub model: String,
    pub instructions: String,
    pub max_tokens: u32,
}

/// Append-only format transition; old headers remain unchanged.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormatUpgradeEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: u64,
    pub version: u32,
}

/// Written and flushed before the summary model is called. An unresolved attempt
/// after cancellation or process failure has no effect on model context.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionStartedEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: u64,
    pub first_kept_entry_id: String,
    pub source_entry_ids: Vec<String>,
    pub request: SummaryRequest,
    pub input_bytes: usize,
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
    pub summary: CompactionSummary,
    pub usage: Usage,
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
}

impl SessionStore {
    /// Effective file format, including appended upgrades.
    pub fn format_version(&self) -> u32 {
        self.format_version
    }

    /// Persist a format upgrade and request metadata before contacting a model.
    pub fn begin_compaction(&mut self, plan: CompactionPlan) -> Result<String> {
        let mut entry = CompactionStartedEntry {
            id: self.new_id(),
            parent_id: self.leaf.clone(),
            timestamp: now_ms(),
            first_kept_entry_id: plan.first_kept_entry_id,
            source_entry_ids: plan.source_entry_ids,
            request: plan.request,
            input_bytes: plan.input_bytes,
        };
        // Validate the plan before even appending an upgrade.
        self.validate_compaction_start(&entry)?;
        self.upgrade_for_v3()?;
        entry.parent_id = self.leaf.clone();
        entry.id = self.new_id();
        self.append(Entry::CompactionStarted(entry))
    }

    pub(super) fn upgrade_for_v3(&mut self) -> Result<()> {
        if self.format_version < 3 {
            self.append(Entry::FormatUpgrade(FormatUpgradeEntry {
                id: self.new_id(),
                parent_id: self.leaf.clone(),
                timestamp: now_ms(),
                version: 3,
            }))?;
        }
        Ok(())
    }

    /// Commit a validated result. Persistence precedes projection changes.
    pub fn append_compaction(&mut self, attempt_id: &str, summary: CompactionSummary, usage: Usage) -> Result<String> {
        let first_kept_entry_id = self.compaction_attempt(attempt_id)?.first_kept_entry_id.clone();
        self.append(Entry::Compaction(CompactionEntry {
            id: self.new_id(),
            parent_id: self.leaf.clone(),
            timestamp: now_ms(),
            attempt_id: attempt_id.into(),
            first_kept_entry_id,
            summary,
            usage,
        }))
    }

    /// Record a failed summary without replacing the current projection.
    pub fn fail_compaction(
        &mut self,
        attempt_id: &str,
        error: String,
        usage: Usage,
        response: Option<AssistantMessage>,
    ) -> Result<String> {
        self.append(Entry::CompactionFailed(CompactionFailedEntry {
            id: self.new_id(),
            parent_id: self.leaf.clone(),
            timestamp: now_ms(),
            attempt_id: attempt_id.into(),
            error,
            usage,
            response,
        }))
    }

    fn compaction_attempt(&self, id: &str) -> Result<&CompactionStartedEntry> {
        match self.get(id) {
            Some(Entry::CompactionStarted(entry)) => Ok(entry),
            _ => bail!("compaction attempt {id:?} not found"),
        }
    }

    pub(super) fn validate_entry(&self, entry: &Entry) -> Result<()> {
        ensure!(
            !entry.id().is_empty() && !self.index.contains_key(entry.id()),
            "duplicate or empty entry ID {:?}",
            entry.id()
        );
        if let Some(parent) = entry.parent_id() {
            ensure!(self.index.contains_key(parent), "entry parent {parent:?} not found");
        }
        match entry {
            Entry::Message(_) | Entry::ContextEdit(_) => {}
            Entry::FormatUpgrade(upgrade) => {
                ensure!(
                    upgrade.version >= self.format_version && upgrade.version <= SESSION_VERSION,
                    "unsupported format upgrade {}",
                    upgrade.version
                );
            }
            Entry::CompactionStarted(started) => {
                ensure!(self.format_version >= 3, "compaction requires a format upgrade to v3");
                self.validate_compaction_start(started)?;
            }
            Entry::Compaction(done) => {
                let source = self.validate_attempt_completion(&done.attempt_id, entry.parent_id())?;
                self.validate_compaction_start_at(source, entry.parent_id())?;
                let path = self.branch_path_from(entry.parent_id());
                let started = path
                    .iter()
                    .position(|entry| entry.id() == done.attempt_id)
                    .expect("validate_attempt_completion located the attempt on this path");
                ensure!(!path[started + 1..].iter().any(|entry| matches!(entry, Entry::ContextEdit(edit) if source.source_entry_ids.contains(&edit.target_id))), "compaction source changed during the summary request");
                ensure!(
                    done.first_kept_entry_id == source.first_kept_entry_id,
                    "compaction retained boundary changed"
                );
                done.summary.validate(&source.source_entry_ids)?;
            }
            Entry::CompactionFailed(failed) => {
                self.validate_attempt_completion(&failed.attempt_id, entry.parent_id())?;
            }
            Entry::RequestConfig(_) | Entry::RequestStarted(_) => self.validate_request_entry(entry)?,
        }
        Ok(())
    }

    fn validate_attempt_completion(&self, id: &str, parent: Option<&str>) -> Result<&CompactionStartedEntry> {
        let path = self.branch_path_from(parent);
        ensure!(
            path.iter().any(|entry| entry.id() == id),
            "compaction attempt is not on this branch"
        );
        ensure!(
            !path.iter().any(|entry| match entry {
                Entry::Compaction(done) => done.attempt_id == id,
                Entry::CompactionFailed(failed) => failed.attempt_id == id,
                Entry::Message(_)
                | Entry::ContextEdit(_)
                | Entry::FormatUpgrade(_)
                | Entry::CompactionStarted(_)
                | Entry::RequestConfig(_)
                | Entry::RequestStarted(_) => false,
            }),
            "compaction attempt already finished"
        );
        self.compaction_attempt(id)
    }

    fn validate_compaction_start(&self, entry: &CompactionStartedEntry) -> Result<()> {
        self.validate_compaction_start_at(entry, entry.parent_id.as_deref())
    }

    fn validate_compaction_start_at(&self, entry: &CompactionStartedEntry, parent: Option<&str>) -> Result<()> {
        let path = self.branch_path_from(parent);
        let raw = Self::raw_projection_from(&path);
        let cut = raw
            .iter()
            .position(|item| item.entry_id == entry.first_kept_entry_id)
            .ok_or_else(|| anyhow::anyhow!("compaction retained entry not on branch"))?;
        ensure!(
            !matches!(raw[cut].message, Message::ToolResult(_) | Message::System(_)),
            "compaction cannot split a tool exchange or retain from a system message"
        );
        ensure!(
            raw[..cut]
                .iter()
                .any(|item| !matches!(item.message, Message::System(_))),
            "no old messages to compact"
        );
        let expected: Vec<_> = raw[..cut].iter().map(|item| &item.entry_id).collect();
        ensure!(
            expected == entry.source_entry_ids.iter().collect::<Vec<_>>(),
            "compaction sources must be the original prefix"
        );
        ensure!(
            entry.request.max_tokens > 0 && !entry.request.model.trim().is_empty(),
            "invalid summary request"
        );
        Ok(())
    }

    /// Original projected messages, applying context edits but ignoring every summary.
    pub fn raw_projection(&self) -> Vec<ProjectedMessage> {
        Self::raw_projection_from(&self.branch_path())
    }

    /// Latest successful, still-valid summary on the active branch.
    pub fn latest_compaction(&self) -> Option<&CompactionEntry> {
        self.latest_compaction_at(self.leaf.as_deref())
    }

    fn latest_compaction_at(&self, leaf: Option<&str>) -> Option<&CompactionEntry> {
        let path = self.branch_path_from(leaf);
        let (position, compact) = path.iter().enumerate().rev().find_map(|(index, entry)| {
            if let Entry::Compaction(entry) = entry {
                Some((index, entry))
            } else {
                None
            }
        })?;
        // Invalid or incomplete control records must never hide original evidence.
        let Some(Entry::CompactionStarted(attempt)) = self.get(&compact.attempt_id) else {
            return None;
        };
        // Editing summarized evidence invalidates the summary; fall back to originals.
        if path[position + 1..].iter().any(
            |entry| matches!(entry, Entry::ContextEdit(edit) if attempt.source_entry_ids.contains(&edit.target_id)),
        ) {
            return None;
        }
        Some(compact)
    }

    pub(super) fn compacted_projection(&self) -> Vec<ProjectedMessage> {
        self.compacted_projection_at(self.leaf.as_deref())
    }

    pub(super) fn compacted_projection_at(&self, leaf: Option<&str>) -> Vec<ProjectedMessage> {
        let raw = Self::raw_projection_from(&self.branch_path_from(leaf));
        let Some(compact) = self.latest_compaction_at(leaf) else {
            return raw;
        };
        let Some(cut) = raw.iter().position(|item| item.entry_id == compact.first_kept_entry_id) else {
            return raw;
        };
        let mut projected: Vec<_> = raw[..cut]
            .iter()
            .filter(|item| matches!(item.message, Message::System(_)))
            .cloned()
            .collect();
        projected.push(ProjectedMessage {
            entry_id: compact.id.clone(),
            message: compact.summary.message(compact.timestamp),
        });
        projected.extend(raw.into_iter().skip(cut));
        projected
    }
}
