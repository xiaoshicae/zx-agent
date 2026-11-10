//! Recording a summary: its attempt before the request, then its result or its failure,
//! and the rules each record follows, both when it is appended and when a session is
//! loaded.

use anyhow::{Result, bail, ensure};

use super::{CompactionEntry, CompactionFailedEntry, CompactionPlan, CompactionStartedEntry, CompactionSummary};
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
        }))
        .await
    }

    /// Commit the result of a summary, validated like loading. Persistence precedes
    /// projection changes.
    pub async fn append_compaction(
        &mut self,
        attempt_id: &str,
        summary: CompactionSummary,
        usage: Usage,
    ) -> Result<String> {
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
    /// source was corrected meanwhile, and the summary cites only what the attempt read.
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
        done.summary.validate(&source.source_entry_ids)
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
