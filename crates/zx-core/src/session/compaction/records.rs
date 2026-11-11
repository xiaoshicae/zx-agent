//! Recording a compaction: its start before the request, then its result or its failure,
//! and the rules each record follows, both when it is appended and when a session is
//! loaded.

use anyhow::{Result, bail, ensure};

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
        let first_kept_entry_id = self.compaction_started(compaction_id)?.first_kept_entry_id.clone();
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

    /// The start record of compaction `id`.
    pub(in crate::session) fn compaction_started(&self, id: &str) -> Result<&CompactionStartedEntry> {
        match self.get(id) {
            Some(Entry::CompactionStarted(entry)) => Ok(entry),
            _ => bail!("compaction {id:?} not found"),
        }
    }

    /// The rules a compaction's result follows: the compaction is still open on this branch,
    /// no source was corrected meanwhile, and the summary cites only what the compaction read.
    pub(in crate::session) fn validate_compaction(&self, done: &CompactionEntry, parent: Option<&str>) -> Result<()> {
        let source = self.validate_open_compaction(&done.compaction_id, parent)?;
        let path = self.branch_path_from(parent);
        let started = path
            .iter()
            .position(|entry| entry.id() == done.compaction_id)
            .expect("validate_open_compaction located the start on this path");
        if let Some(corrected) = Self::corrected_source(&path[started + 1..], &source.source_entry_ids) {
            bail!(
                "compaction {}: source {corrected} was corrected during the summary request",
                done.compaction_id
            );
        }
        ensure!(
            done.first_kept_entry_id == source.first_kept_entry_id,
            "compaction {}: the result keeps from entry {} but the start from {}",
            done.compaction_id,
            done.first_kept_entry_id,
            source.first_kept_entry_id
        );
        done.summary.validate(&source.source_entry_ids)
    }

    /// The start record of compaction `id`, which must be open: on this branch and not
    /// finished yet.
    pub(in crate::session) fn validate_open_compaction(
        &self,
        id: &str,
        parent: Option<&str>,
    ) -> Result<&CompactionStartedEntry> {
        let path = self.branch_path_from(parent);
        ensure!(
            path.iter().any(|entry| entry.id() == id),
            "the compaction is not on this branch"
        );
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
        self.compaction_started(id)
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
        let raw = Self::raw_projection_from(&self.branch_path_from(entry.parent_id.as_deref()));
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
