//! Recording a compaction: its start before the request, then its result or its failure,
//! and the rules each record follows, both when it is appended and when a session is
//! loaded.

use anyhow::{Result, anyhow, bail, ensure};

use super::{CompactionEntry, CompactionFailedEntry, CompactionPlan, CompactionStartedEntry, CompactionSummary};
use crate::ai::{AssistantMessage, Message, Usage, now_ms};
use crate::session::{Entry, SessionStore};

impl SessionStore {
    /// Persist the start and its request metadata before contacting a model. Fails
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
        }))
        .await
    }

    /// Commit the result of a summary, validated like loading. Persistence precedes
    /// projection changes.
    pub async fn append_compaction(
        &mut self,
        compaction_id: &str,
        summary: CompactionSummary,
        usage: Usage,
    ) -> Result<String> {
        let (_, started) = Self::compaction_start_on(&self.branch_path(), compaction_id)?;
        let first_kept_entry_id = started.first_kept_entry_id.clone();
        self.append(Entry::Compaction(CompactionEntry {
            id: self.new_id(),
            parent_id: self.leaf.clone(),
            timestamp: now_ms(),
            compaction_id: compaction_id.into(),
            first_kept_entry_id,
            summary,
            usage,
        }))
        .await
    }

    /// Record a failed summary without replacing the current projection.
    pub async fn fail_compaction(
        &mut self,
        compaction_id: &str,
        error: String,
        usage: Usage,
        response: Option<AssistantMessage>,
    ) -> Result<String> {
        self.append(Entry::CompactionFailed(CompactionFailedEntry {
            id: self.new_id(),
            parent_id: self.leaf.clone(),
            timestamp: now_ms(),
            compaction_id: compaction_id.into(),
            error,
            usage,
            response,
        }))
        .await
    }

    /// The start record of compaction `id` and its position on `path`. An error when the
    /// compaction did not start on this branch.
    pub(in crate::session) fn compaction_start_on<'s>(
        path: &[&'s Entry],
        id: &str,
    ) -> Result<(usize, &'s CompactionStartedEntry)> {
        path.iter()
            .enumerate()
            .find_map(|(position, entry)| match entry {
                Entry::CompactionStarted(started) if started.id == id => Some((position, started)),
                _ => None,
            })
            .ok_or_else(|| anyhow!("compaction {id} is not on this branch"))
    }

    /// As [`Self::compaction_start_on`], for a compaction that is still open: neither its
    /// result nor its failure is on `path`.
    fn open_compaction_on<'s>(path: &[&'s Entry], id: &str) -> Result<(usize, &'s CompactionStartedEntry)> {
        let found = Self::compaction_start_on(path, id)?;
        ensure!(
            !path.iter().any(|entry| match entry {
                Entry::Compaction(done) => done.compaction_id == id,
                Entry::CompactionFailed(failed) => failed.compaction_id == id,
                Entry::Message(_)
                | Entry::ContextEdit(_)
                | Entry::CompactionStarted(_)
                | Entry::RequestConfig(_)
                | Entry::RequestStarted(_)
                | Entry::ToolCallStarted(_)
                | Entry::Custom(_)
                | Entry::CustomMessage(_)
                | Entry::SessionInfo(_) => false,
            }),
            "the compaction already finished"
        );
        Ok(found)
    }

    /// The rules the start of a compaction follows when it is recorded: its plan matches the
    /// branch.
    pub(in crate::session) fn validate_compaction_start(&self, entry: &CompactionStartedEntry) -> Result<()> {
        ensure!(
            entry.request.max_tokens > 0,
            "compaction {}: maxTokens must be greater than zero",
            entry.id
        );
        ensure!(
            !entry.request.model.trim().is_empty(),
            "compaction {}: the model is empty",
            entry.id
        );
        let source = self.compaction_source(entry.parent_id.as_deref(), &entry.first_kept_entry_id)?;
        ensure!(
            source.iter().any(|item| !matches!(item.message, Message::System(_))),
            "no old messages to compact"
        );
        ensure!(
            source.iter().map(|item| &item.entry_id).eq(&entry.source_entry_ids),
            "compaction sources must be the original prefix"
        );
        Ok(())
    }

    /// The rules a compaction's result follows: the compaction is still open on this branch,
    /// no source was corrected meanwhile, and the summary cites only what the compaction read.
    pub(in crate::session) fn validate_compaction(&self, done: &CompactionEntry) -> Result<()> {
        let path = self.branch_path_from(done.parent_id.as_deref());
        let (position, started) = Self::open_compaction_on(&path, &done.compaction_id)?;
        if let Some(corrected) = Self::corrected_source(&path[position + 1..], &started.source_entry_ids) {
            bail!(
                "compaction {}: source {corrected} was corrected during the summary request",
                done.compaction_id
            );
        }
        ensure!(
            done.first_kept_entry_id == started.first_kept_entry_id,
            "compaction {}: the result keeps from entry {} but the start from {}",
            done.compaction_id,
            done.first_kept_entry_id,
            started.first_kept_entry_id
        );
        done.summary.validate(&started.source_entry_ids)
    }

    /// The rule a compaction's failure follows: the compaction is still open on this branch.
    pub(in crate::session) fn validate_compaction_failure(&self, failed: &CompactionFailedEntry) -> Result<()> {
        let path = self.branch_path_from(failed.parent_id.as_deref());
        Self::open_compaction_on(&path, &failed.compaction_id)?;
        Ok(())
    }
}
