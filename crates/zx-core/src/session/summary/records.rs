//! Recording a summary: its attempt before the request, then its result or its failure,
//! and the rules each record follows, both when it is appended and when a session is
//! loaded.

use anyhow::{Result, bail, ensure};

use super::{
    CompactionEntry, CompactionFailedEntry, CompactionPlan, CompactionStartedEntry, CompactionSummary, MergedSummary,
    SummaryFact, SummaryMode, cite_only, superseded_by,
};
use crate::ai::{AssistantMessage, Message, Usage, now_ms};
use crate::session::{Entry, SessionStore};

impl SessionStore {
    /// Persist the attempt and its request metadata before contacting a model. Fails
    /// without writing anything if the plan does not match the branch.
    pub async fn begin_compaction(&mut self, plan: CompactionPlan) -> Result<String> {
        self.append(Entry::CompactionStarted(CompactionStartedEntry {
            id: self.new_id(),
            parent_id: self.leaf.clone(),
            timestamp: now_ms(),
            first_kept_entry_id: plan.first_kept_entry_id,
            source_entry_ids: plan.source_entry_ids,
            request: plan.request,
            input_bytes: plan.input_bytes,
            mode: plan.mode,
            continues: plan.continues,
        }))
        .await
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

    /// The rules a summary result follows: its attempt is still open on this branch, no
    /// source was corrected meanwhile, and the summary cites and carries only what the
    /// attempt allows.
    pub(in crate::session) fn validate_compaction(&self, done: &CompactionEntry, parent: Option<&str>) -> Result<()> {
        let source = self.validate_attempt_completion(&done.attempt_id, parent)?;
        let path = self.branch_path_from(parent);
        let started = path
            .iter()
            .position(|entry| entry.id() == done.attempt_id)
            .expect("validate_attempt_completion located the attempt on this path");
        if let Some(corrected) = Self::corrected_source(&path[started + 1..], &source.source_entry_ids) {
            bail!(
                "compaction attempt {}: source {corrected} was corrected during the summary request",
                done.attempt_id
            );
        }
        ensure!(
            done.first_kept_entry_id == source.first_kept_entry_id,
            "compaction attempt {}: the result keeps from entry {} but the attempt from {}",
            done.attempt_id,
            done.first_kept_entry_id,
            source.first_kept_entry_id
        );
        match source.mode {
            SummaryMode::Full => {
                ensure!(
                    done.superseded.is_empty() && done.evicted.is_empty(),
                    "a full summary supersedes and evicts nothing"
                );
                done.summary.validate(&source.source_entry_ids)?;
            }
            SummaryMode::Incremental => self.validate_merged(source, done)?,
        }
        Ok(())
    }

    pub(in crate::session) fn validate_attempt_completion(
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
                | Entry::CompactionStarted(_)
                | Entry::RequestConfig(_)
                | Entry::RequestStarted(_)
                | Entry::ToolCallStarted(_)
                | Entry::Custom(_)
                | Entry::CustomMessage(_) => false,
            }),
            "compaction attempt already finished"
        );
        self.compaction_attempt(id)
    }

    /// The rules a summary attempt follows when it is recorded: its plan matches the branch.
    pub(in crate::session) fn validate_compaction_start(&self, entry: &CompactionStartedEntry) -> Result<()> {
        self.validate_compaction_start_at(entry, entry.parent_id.as_deref())
    }

    fn validate_compaction_start_at(&self, entry: &CompactionStartedEntry, parent: Option<&str>) -> Result<()> {
        ensure!(
            entry.request.max_tokens > 0,
            "summary attempt {}: maxTokens must be greater than zero",
            entry.id
        );
        ensure!(
            !entry.request.model.trim().is_empty(),
            "summary attempt {}: the model is empty",
            entry.id
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
}
