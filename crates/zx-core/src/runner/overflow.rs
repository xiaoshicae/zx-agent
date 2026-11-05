//! Recovery from a request the model rejected as too long for its context (pi: the overflow
//! branch of AgentSession's post-run recovery, coding-agent/src/core/agent-session.ts).

use tokio_util::sync::CancellationToken;

use super::accounting::RunAccounting;
use super::omit_failed_attempt;
use crate::agent::{AgentEvent, Emit, HookChain, LoopOutcome};
use crate::ai::Provider;
use crate::compaction::{self, Trigger};
use crate::session::SessionStore;

/// What one overflow recovery borrows from the run.
pub(super) struct OverflowRecovery<'a> {
    pub session: &'a mut SessionStore,
    pub hooks: &'a HookChain,
    pub to: Emit<'a>,
    pub provider: &'a dyn Provider,
    pub model: &'a str,
    pub compaction: &'a compaction::Config,
    pub compaction_pause: &'a mut Option<String>,
    /// The attempt that failed, for the events of the summary.
    pub attempt: u32,
    pub cancel: &'a CancellationToken,
}

impl OverflowRecovery<'_> {
    /// Omit the failed answer from the context and summarize the old prefix, so that a new
    /// attempt can continue from the compacted context. The summary's requests and usage are
    /// added to `accounting` whether or not it succeeds. Returns how the run ends when the
    /// context cannot be compacted, with the reason.
    pub async fn run(
        self,
        error: &str,
        failed_entry: Option<&str>,
        accounting: &mut RunAccounting,
    ) -> Result<(), (LoopOutcome, String)> {
        let overflow = AgentEvent::ContextOverflow { error: error.into() };
        self.hooks.emit(overflow, self.to).await;
        if let Some(target) = failed_entry {
            omit_failed_attempt(self.session, self.hooks, self.to, target)
                .await
                .map_err(|error| (LoopOutcome::Error, error))?;
        }
        let mut host = compaction::Host {
            session: &mut *self.session,
            provider: self.provider,
            model: self.model,
            config: self.compaction,
            to: self.to,
            hooks: self.hooks,
            attempt: self.attempt,
        };
        let (report, messages) = host.run(Trigger::Overflow, self.cancel).await;
        accounting.model_requests = accounting.model_requests.saturating_add(report.requests);
        accounting.usage.add(&report.usage);
        if let Some(error) = report.persistence_error {
            return Err((LoopOutcome::Error, error));
        }
        if messages.is_some() {
            return Ok(());
        }
        if report.outcome == compaction::Outcome::Aborted {
            return Err((LoopOutcome::Aborted, "run was cancelled".into()));
        }
        let reason = report.error.unwrap_or_else(|| "compaction made no progress".into());
        // As for an automatic summary: a paid summary request failed, so later prompts do not
        // pay for the same failure again.
        if report.outcome == compaction::Outcome::Failed && report.requests > 0 {
            *self.compaction_pause = Some(reason.clone());
        }
        Err((
            LoopOutcome::Error,
            format!("{error}; compacting the context to recover failed: {reason}"),
        ))
    }
}
