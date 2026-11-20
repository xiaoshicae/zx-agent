//! What follows a provider failure: whether the run tries again, and how. A retry waits
//! and asks again; a context the model rejected as too long is compacted once instead.
//! Either way the failed answer stays in the session and leaves the context.

use super::run_scope::{RunScope, RunStop};
use super::{Ending, RunAccounting, Running};
use crate::ai::retry::{RetryPolicy, is_context_overflow, is_retryable};
use crate::ai::{AssistantMessage, duration_ms};
use crate::compaction::End;
use crate::events::EventKind;

impl Running<'_> {
    /// Recover from a context overflow by compacting once, or schedule a retry; an error
    /// says why the run ends instead. Both are requests, so both need budget. `failed` is
    /// the response that failed, in the session as `entry_id`; `task_steps` counts the steps
    /// of its attempt.
    pub(super) async fn after_failure(
        &mut self,
        failed: &AssistantMessage,
        entry_id: &str,
        task_steps: u32,
    ) -> Result<(), RunStop> {
        let error = failed.failure_reason();
        let cancelled = self.scope.is_cancelled();
        let no_further_request = self.accounting.exhausted().is_some() || cancelled;
        if is_context_overflow(failed) && self.scope.compacts_automatically() {
            // A context the model rejects as too long fails again however often it is sent:
            // compact once and continue. Not a retry, so it has its own limit: once per run,
            // until an answer succeeds again (more than one step in the attempt).
            let refusal = if self.overflow_recovered && task_steps <= 1 {
                Some("the context still overflowed after compacting it once".to_string())
            } else {
                self.scope.pause.refusal()
            };
            if let Some(refusal) = refusal {
                return Err(RunStop::Failed(format!("{error}; {refusal}")));
            }
            if no_further_request {
                return Err(RunStop::Failed(error.to_string()));
            }
            self.scope
                .recover_overflow(error, entry_id, &mut self.accounting)
                .await?;
            self.overflow_recovered = true;
            return Ok(());
        }
        // Without budget, the provider error is the reason the run ends.
        if !is_retryable(failed) || self.accounting.retries >= self.retry.max_retries || no_further_request {
            return Err(RunStop::Failed(error.to_string()));
        }
        self.scope
            .schedule_retry(self.retry, &mut self.accounting, failed, entry_id)
            .await
    }
}

impl RunScope<'_> {
    /// Keep the failed answer `entry_id` in the session but drop it from the model context, so
    /// that the next attempt continues from the message before it without repeating the user
    /// message.
    pub(super) async fn omit_answer(&mut self, entry_id: &str) -> Result<(), String> {
        let index = self
            .transcript
            .projection()
            .iter()
            .position(|item| item.entry_id == entry_id);
        let edit = self
            .transcript
            .append_context_edit(entry_id, None)
            .await
            .map_err(|error| format!("cannot omit the failed attempt: {error:#}"))?;
        if let Some(index) = index {
            self.target
                .emit(EventKind::ContextEdited {
                    index,
                    replacement: None,
                    entry_id: edit,
                })
                .await;
        }
        Ok(())
    }

    /// Omit the failed answer and summarize the old prefix, so that a new attempt can continue
    /// from the compacted context. The summary's cost is added to `accounting` whether or not
    /// it succeeds.
    pub(super) async fn recover_overflow(
        &mut self,
        error: &str,
        failed_entry: &str,
        accounting: &mut RunAccounting,
    ) -> Result<(), RunStop> {
        self.target
            .emit(EventKind::ContextOverflow { error: error.into() })
            .await;
        self.omit_answer(failed_entry).await.map_err(RunStop::Failed)?;
        let compacted = self.compact_for_run(accounting).await;
        let reason = match compacted.end {
            End::Completed { .. } => return Ok(()),
            End::Cancelled(_) => return Err(RunStop::Ended(Ending::Cancelled)),
            End::Unrecorded(error) => return Err(RunStop::Failed(format!("{error:#}"))),
            End::Failed(reason) => reason,
            End::Noop => "the model rejected the context as too long but cannot compact while preserving \
                          system messages and the latest input; lower keep_recent_bytes, use a model with a \
                          larger context or start a new session"
                .into(),
        };
        Err(RunStop::Failed(format!(
            "{error}; compacting the context to recover failed: {reason}"
        )))
    }

    /// Omit the failed answer from the context, then count and announce the retry and wait
    /// out the backoff: as every step, a retry is recorded before it is announced. Ends the
    /// run when the session cannot record the edit or the wait is cancelled.
    pub(super) async fn schedule_retry(
        &mut self,
        retry: &RetryPolicy,
        accounting: &mut RunAccounting,
        failed: &AssistantMessage,
        failed_entry: &str,
    ) -> Result<(), RunStop> {
        self.omit_answer(failed_entry).await.map_err(RunStop::Failed)?;
        accounting.retries = accounting.retries.saturating_add(1);
        let delay = retry.delay_for(accounting.retries, failed);
        self.target
            .emit(EventKind::RetryScheduled {
                retry: accounting.retries,
                max_retries: retry.max_retries,
                delay_ms: duration_ms(delay),
                error: failed.failure_reason().to_string(),
            })
            .await;
        tokio::select! {
            () = tokio::time::sleep(delay) => Ok(()),
            () = self.cancel().cancelled() => Err(RunStop::Ended(Ending::Cancelled)),
        }
    }
}
