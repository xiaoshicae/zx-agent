//! Versioned compaction records and validation; all mutations append to the tree.

use std::collections::HashSet;

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::ai::{AssistantMessage, Message, PREAMBLE, SystemMessage, SystemPrompt, Usage, UserMessageKind, now_ms};

use super::{COMPACTION_VERSION, Entry, INCREMENTAL_SUMMARY_VERSION, ProjectedMessage, SessionStore, without_details};

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
    /// summary (P5). A summary whose sources are corrected is invalid as a whole. Reading is
    /// bounded at 2 MiB, so a long session eventually cannot be summarized.
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
            "summary exceeds 65536 bytes"
        );
        ensure!(
            self.facts.len() <= MAX_STATEMENTS
                && self.inferences.len() <= MAX_STATEMENTS
                && self.plans.len() <= MAX_STATEMENTS,
            "too many summary statements"
        );
        ensure!(
            !self.facts.is_empty() || !self.inferences.is_empty() || !self.plans.is_empty(),
            "summary is empty"
        );
        for fact in &self.facts {
            ensure!(!fact.text.trim().is_empty(), "summary fact is empty");
            ensure!(!fact.source_entry_ids.is_empty(), "summary fact has no sourceEntryIds");
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

impl SummaryRequest {
    /// The messages of the request as it was sent: the instructions as the system prompt,
    /// `input` as the user message.
    pub(crate) fn messages(&self, input: String, system_timestamp: u64, user_timestamp: u64) -> Vec<Message> {
        summary_messages(&self.instructions, input, system_timestamp, user_timestamp)
    }
}

/// The two messages of a summary request, built the same way when it is sent and when it is
/// replayed from the session.
pub(crate) fn summary_messages(
    instructions: &str,
    input: String,
    system_timestamp: u64,
    user_timestamp: u64,
) -> Vec<Message> {
    vec![
        Message::System(SystemMessage {
            sections: SystemPrompt::default().diff(&SystemPrompt::default().with_section(PREAMBLE, instructions)),
            timestamp: system_timestamp,
        }),
        Message::User {
            content: input,
            timestamp: user_timestamp,
            kind: UserMessageKind::Prompt,
        },
    ]
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

/// What an incremental summary that continues an earlier one reads: that summary's facts
/// that still hold, and the records to summarize.
pub(crate) struct IncrementalSource {
    pub previous_facts: Vec<SummaryFact>,
    pub records: Vec<ProjectedMessage>,
}

/// One earlier fact as the summary model sees it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PreviousFact<'a> {
    id: &'a str,
    text: &'a str,
    source_entry_ids: &'a [String],
}

/// The content of an incremental summary request.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct IncrementalInput<'a> {
    previous_facts: Vec<PreviousFact<'a>>,
    records: &'a [ProjectedMessage],
}

impl IncrementalSource {
    /// The request content: `{"previousFacts": [...], "records": [...]}`. Every previous
    /// fact has an ID: the effective facts of a summary are always numbered.
    pub(crate) fn input(&self) -> Result<String> {
        let previous_facts = self
            .previous_facts
            .iter()
            .map(|fact| PreviousFact {
                id: fact.id.as_deref().unwrap_or_default(),
                text: &fact.text,
                source_entry_ids: &fact.source_entry_ids,
            })
            .collect();
        Ok(serde_json::to_string(&IncrementalInput {
            previous_facts,
            records: &without_details(&self.records),
        })?)
    }
}

impl SessionStore {
    /// Persist a format upgrade and request metadata before contacting a model. An
    /// incremental summary needs format v5; a full one, v3.
    pub async fn begin_compaction(&mut self, plan: CompactionPlan) -> Result<String> {
        let mut entry = CompactionStartedEntry {
            id: self.new_id(),
            parent_id: self.leaf.clone(),
            timestamp: now_ms(),
            first_kept_entry_id: plan.first_kept_entry_id,
            source_entry_ids: plan.source_entry_ids,
            request: plan.request,
            input_bytes: plan.input_bytes,
            mode: plan.mode,
            continues: plan.continues,
        };
        // Validate the plan before even appending an upgrade.
        self.validate_compaction_start(&entry)?;
        let version = match entry.mode {
            SummaryMode::Full => COMPACTION_VERSION,
            SummaryMode::Incremental => INCREMENTAL_SUMMARY_VERSION,
        };
        self.ensure_format(version).await?;
        entry.parent_id = self.leaf.clone();
        entry.id = self.new_id();
        self.append(Entry::CompactionStarted(entry)).await
    }

    /// Commit a validated full summary. Persistence precedes projection changes.
    pub async fn append_compaction(
        &mut self,
        attempt_id: &str,
        summary: CompactionSummary,
        usage: Usage,
    ) -> Result<String> {
        self.append_merged_compaction(
            attempt_id,
            MergedSummary {
                summary,
                superseded: Vec::new(),
                evicted: Vec::new(),
            },
            usage,
        )
        .await
    }

    /// Commit the result of an incremental summary: the carried facts that still hold, then
    /// the new ones, and the earlier facts it no longer carries. Validated like loading.
    pub async fn append_merged_compaction(
        &mut self,
        attempt_id: &str,
        merged: MergedSummary,
        usage: Usage,
    ) -> Result<String> {
        let first_kept_entry_id = self.compaction_attempt(attempt_id)?.first_kept_entry_id.clone();
        self.append(Entry::Compaction(CompactionEntry {
            id: self.new_id(),
            parent_id: self.leaf.clone(),
            timestamp: now_ms(),
            attempt_id: attempt_id.into(),
            first_kept_entry_id,
            summary: merged.summary,
            usage,
            superseded: merged.superseded,
            evicted: merged.evicted,
        }))
        .await
    }

    /// Record a failed summary without replacing the current projection.
    pub async fn fail_compaction(
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
        .await
    }

    fn compaction_attempt(&self, id: &str) -> Result<&CompactionStartedEntry> {
        match self.get(id) {
            Some(Entry::CompactionStarted(entry)) => Ok(entry),
            _ => bail!("compaction attempt {id:?} not found"),
        }
    }

    /// The rules a summary attempt follows when it is recorded: the format allows it and its
    /// plan matches the branch.
    pub(super) fn validate_compaction_started(&self, started: &CompactionStartedEntry) -> Result<()> {
        ensure!(
            self.format_version >= COMPACTION_VERSION,
            "compaction requires a format upgrade to v{COMPACTION_VERSION}"
        );
        ensure!(
            started.mode == SummaryMode::Full || self.format_version >= INCREMENTAL_SUMMARY_VERSION,
            "incremental summaries require a format upgrade to v{INCREMENTAL_SUMMARY_VERSION}"
        );
        self.validate_compaction_start(started)
    }

    /// The rules a summary result follows: its attempt is still open on this branch, no
    /// source was corrected meanwhile, and the summary cites and carries only what the
    /// attempt allows.
    pub(super) fn validate_compaction(&self, done: &CompactionEntry, parent: Option<&str>) -> Result<()> {
        let source = self.validate_attempt_completion(&done.attempt_id, parent)?;
        let path = self.branch_path_from(parent);
        let started = path
            .iter()
            .position(|entry| entry.id() == done.attempt_id)
            .expect("validate_attempt_completion located the attempt on this path");
        ensure!(
            !Self::any_source_corrected(&path[started + 1..], &source.source_entry_ids),
            "compaction source changed during the summary request"
        );
        ensure!(
            done.first_kept_entry_id == source.first_kept_entry_id,
            "compaction retained boundary changed"
        );
        match source.mode {
            SummaryMode::Full => {
                self.validate_compaction_start_at(source, parent)?;
                ensure!(
                    done.superseded.is_empty() && done.evicted.is_empty(),
                    "a full summary supersedes and evicts nothing"
                );
                done.summary.validate(&source.source_entry_ids)?;
            }
            SummaryMode::Incremental => {
                self.validate_compaction_start(source)?;
                self.validate_merged(source, done)?;
            }
        }
        Ok(())
    }

    pub(super) fn validate_attempt_completion(
        &self,
        id: &str,
        parent: Option<&str>,
    ) -> Result<&CompactionStartedEntry> {
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
                | Entry::RequestStarted(_)
                | Entry::ToolCallStarted(_) => false,
            }),
            "compaction attempt already finished"
        );
        self.compaction_attempt(id)
    }

    fn validate_compaction_start(&self, entry: &CompactionStartedEntry) -> Result<()> {
        self.validate_compaction_start_at(entry, entry.parent_id.as_deref())
    }

    fn validate_compaction_start_at(&self, entry: &CompactionStartedEntry, parent: Option<&str>) -> Result<()> {
        ensure!(
            entry.request.max_tokens > 0 && !entry.request.model.trim().is_empty(),
            "invalid summary request"
        );
        let path = self.branch_path_from(parent);
        match (entry.mode, &entry.continues) {
            (SummaryMode::Full, Some(_)) => bail!("only an incremental summary continues another"),
            (SummaryMode::Incremental, Some(previous)) => {
                let source = self.recorded_incremental_source(&path, previous, &entry.first_kept_entry_id)?;
                let expected: Vec<&String> = source.records.iter().map(|item| &item.entry_id).collect();
                ensure!(
                    expected == entry.source_entry_ids.iter().collect::<Vec<_>>(),
                    "incremental summary sources must be the corrected earlier records and those after the previous summary"
                );
                Ok(())
            }
            (_, None) => {
                let raw = Self::raw_projection_from(&path);
                let cut = Self::retained_from(&raw, &entry.first_kept_entry_id)?;
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
                Ok(())
            }
        }
    }

    /// Where the retained messages start in `raw`: at `first_kept`, which must not split a
    /// tool exchange or be a system message.
    pub(super) fn retained_from(raw: &[ProjectedMessage], first_kept: &str) -> Result<usize> {
        let cut = raw
            .iter()
            .position(|item| item.entry_id == first_kept)
            .ok_or_else(|| anyhow::anyhow!("compaction retained entry not on branch"))?;
        ensure!(
            !matches!(raw[cut].message, Message::ToolResult(_) | Message::System(_)),
            "compaction cannot split a tool exchange or retain from a system message"
        );
        Ok(cut)
    }

    /// The records before `first_kept`: what a full summary read.
    pub(super) fn summarized_prefix<'a>(
        raw: &'a [ProjectedMessage],
        first_kept: &str,
    ) -> Result<&'a [ProjectedMessage]> {
        let cut = Self::retained_from(raw, first_kept)?;
        Ok(&raw[..cut])
    }

    /// An incremental result: the new facts are numbered after the attempt, in order, and
    /// cite only what it read; the earlier facts that still held are carried over verbatim,
    /// except those the new facts supersede and the oldest ones evicted; all within the limits.
    fn validate_merged(&self, attempt: &CompactionStartedEntry, done: &CompactionEntry) -> Result<()> {
        done.summary.validate_limits()?;
        let previous = match &attempt.continues {
            Some(previous) => {
                let path = self.branch_path_from(attempt.parent_id.as_deref());
                self.recorded_incremental_source(&path, previous, &attempt.first_kept_entry_id)?
                    .previous_facts
            }
            None => Vec::new(),
        };
        let prefix = format!("{}-", attempt.id);
        let split = done
            .summary
            .facts
            .iter()
            .position(|fact| fact.id.as_deref().is_some_and(|id| id.starts_with(&prefix)))
            .unwrap_or(done.summary.facts.len());
        let (carried, new) = done.summary.facts.split_at(split);
        for (index, fact) in new.iter().enumerate() {
            ensure!(
                fact.id.as_deref() == Some(format!("{prefix}{}", index + 1).as_str()),
                "the new facts must be numbered {prefix}1, {prefix}2, ... in order"
            );
        }
        cite_only(new, &attempt.source_entry_ids)?;
        let superseded = superseded_by(&previous, new)?;
        ensure!(
            done.superseded == superseded,
            "superseded facts must be exactly those the new facts name"
        );
        let remaining: Vec<&SummaryFact> = previous
            .iter()
            .filter(|fact| !fact.id.as_ref().is_some_and(|id| superseded.contains(id)))
            .collect();
        ensure!(
            done.evicted.len() <= remaining.len()
                && remaining
                    .iter()
                    .zip(&done.evicted)
                    .all(|(fact, id)| fact.id.as_ref() == Some(id)),
            "evicted facts must be the oldest earlier facts"
        );
        let kept = &remaining[done.evicted.len()..];
        ensure!(
            carried.len() == kept.len() && carried.iter().zip(kept).all(|(fact, earlier)| fact == *earlier),
            "earlier facts must be carried over unchanged"
        );
        Ok(())
    }

    /// What an incremental summary at the end of `path` reads when it continues `previous`,
    /// the latest summary on the path, and keeps the records from `first_kept` on: the facts
    /// of `previous` that still hold, then the records it summarized that were corrected
    /// since and are still visible, then the records after its boundary. `None` when no
    /// message other than a system message follows the previous boundary: nothing to add.
    pub(crate) fn incremental_source_at(
        &self,
        path: &[&Entry],
        previous: &str,
        first_kept: &str,
    ) -> Result<Option<IncrementalSource>> {
        let latest = self
            .latest_compaction_on(path)
            .ok_or_else(|| anyhow::anyhow!("there is no summary to continue on this branch"))?;
        ensure!(
            latest.compaction.id == previous,
            "an incremental summary continues the latest summary on its branch"
        );
        let raw = Self::raw_projection_from(path);
        let old_cut = Self::summarized_count(path, &raw, &latest.compaction.first_kept_entry_id)?;
        let new_cut = Self::retained_from(&raw, first_kept)?;
        if new_cut <= old_cut
            || !raw[old_cut..new_cut]
                .iter()
                .any(|item| !matches!(item.message, Message::System(_)))
        {
            return Ok(None);
        }
        let corrected = Self::corrected_after(path, latest.position);
        let records = raw[..old_cut]
            .iter()
            .filter(|item| corrected.contains(item.entry_id.as_str()))
            .chain(&raw[old_cut..new_cut])
            .cloned()
            .collect();
        Ok(Some(IncrementalSource {
            previous_facts: Self::effective_facts(path, &latest),
            records,
        }))
    }

    /// [`Self::incremental_source_at`] for a recorded attempt, which must add something.
    pub(super) fn recorded_incremental_source(
        &self,
        path: &[&Entry],
        previous: &str,
        first_kept: &str,
    ) -> Result<IncrementalSource> {
        self.incremental_source_at(path, previous, first_kept)?
            .ok_or_else(|| anyhow::anyhow!("no new records to summarize after the previous summary"))
    }
}
