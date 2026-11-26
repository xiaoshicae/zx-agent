//! Recording a compaction: its start before the request, then its result or its failure,
//! and the rules each record follows, both when it is appended and when a session is
//! loaded.

use anyhow::{Context, Result, anyhow, bail, ensure};

use super::{
    CompactionEntry, CompactionFailedEntry, CompactionPlan, CompactionStartedEntry, CompactionSummary, SummarizedBy,
};
use crate::ai::{AssistantMessage, Message, Usage, now_ms};
use crate::transcript::{Entry, Transcript};

impl Transcript {
    /// Persist the start, with who makes the summary, before the summary is asked for.
    /// Fails without writing anything if the plan does not match the branch.
    pub(crate) async fn append_compaction_started(&mut self, plan: CompactionPlan) -> Result<String> {
        self.append(Entry::CompactionStarted(CompactionStartedEntry {
            id: self.new_id(),
            parent_id: self.leaf.clone(),
            timestamp: now_ms(),
            first_kept_entry_id: plan.first_kept_entry_id,
            kept_input_entry_id: plan.kept_input_entry_id,
            source_entry_ids: plan.source_entry_ids,
            by: plan.by,
            input_bytes: plan.input_bytes,
        }))
        .await
    }

    /// Commit the result of a summary, validated like loading: an error, and nothing
    /// written, when the compaction is not open on this branch. Persistence precedes
    /// projection changes.
    pub(crate) async fn append_compaction(
        &mut self,
        compaction_id: &str,
        summary: CompactionSummary,
        usage: Usage,
    ) -> Result<String> {
        self.append(Entry::Compaction(CompactionEntry {
            id: self.new_id(),
            parent_id: self.leaf.clone(),
            timestamp: now_ms(),
            compaction_id: compaction_id.into(),
            summary,
            usage,
        }))
        .await
    }

    /// Record a failed summary without replacing the current projection.
    pub(crate) async fn append_compaction_failed(
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
    pub(in crate::transcript) fn compaction_start_on<'s>(
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
            "compaction {id} already finished"
        );
        Ok(found)
    }

    /// The rules the start of a compaction follows when it is recorded: a summarizer that
    /// makes the summary has a name, and its plan matches the branch.
    pub(in crate::transcript) fn validate_compaction_start(&self, start: &CompactionStartedEntry) -> Result<()> {
        match &start.by {
            // The model and the output limit of a request are in its request configuration,
            // which has its own rules.
            SummarizedBy::Request(_) => {}
            SummarizedBy::Summarizer(name) => ensure!(
                !name.trim().is_empty(),
                "compaction {}: the summarizer has no name",
                start.id
            ),
        }
        let source = self
            .compaction_source(start.parent_id.as_deref(), &start.first_kept_entry_id)
            .with_context(|| format!("compaction {}", start.id))?;
        ensure!(
            source.iter().any(|item| !matches!(item.message, Message::System(_))),
            "compaction {}: no old messages to compact",
            start.id
        );
        ensure!(
            source.iter().map(|item| &item.entry_id).eq(&start.source_entry_ids),
            "compaction {}: its sources are not the records before its boundary",
            start.id
        );
        if let Some(kept) = &start.kept_input_entry_id {
            let latest = source
                .iter()
                .rev()
                .find(|item| matches!(item.message, Message::User { .. }));
            ensure!(
                latest.is_some_and(|item| item.entry_id == *kept),
                "compaction {}: the input it keeps, {kept}, is not the latest input before its boundary",
                start.id
            );
        }
        Ok(())
    }

    /// The rules a compaction's result follows: the compaction is still open on this branch,
    /// no source was corrected meanwhile, and the summary cites only what the compaction read.
    pub(in crate::transcript) fn validate_compaction(&self, done: &CompactionEntry) -> Result<()> {
        let path = self.branch_path_from(done.parent_id.as_deref());
        let (position, started) = Self::open_compaction_on(&path, &done.compaction_id)?;
        if let Some(corrected) = Self::corrected_source(&path[position + 1..], &started.source_entry_ids) {
            bail!(
                "compaction {}: source {corrected} was corrected during the summary request",
                done.compaction_id
            );
        }
        done.summary.validate(&started.source_entry_ids)
    }

    /// The rule a compaction's failure follows: the compaction is still open on this branch.
    pub(in crate::transcript) fn validate_compaction_failure(&self, failed: &CompactionFailedEntry) -> Result<()> {
        let path = self.branch_path_from(failed.parent_id.as_deref());
        Self::open_compaction_on(&path, &failed.compaction_id)?;
        Ok(())
    }
}
