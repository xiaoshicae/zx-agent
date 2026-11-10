//! What the loop needs from the session it runs in: the session, where it records each step
//! before acting on it and whose projection it sends the model, and a chance to prepare that
//! context before a request. A [`SessionStore`] is a host on its own; the Runner's host adds
//! summaries.

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::{AgentEvent, EventTarget, Halt, LoopConfig};
use crate::ai::Usage;
use crate::session::SessionStore;

/// Lends the loop the session it records in, before it acts on each step, and whose
/// projection is the context it sends. When the session cannot record, the loop stops at
/// once ([`Halt::Unrecorded`]), because an unattended run must not keep acting without an
/// audit trail.
#[async_trait]
pub(crate) trait SessionHost: Send + Sync {
    /// The session the loop records in.
    fn session(&mut self) -> &mut SessionStore;

    /// Prepare the context before a task request, for example by summarizing it; what it
    /// records is in the projection afterwards. Request costs, failed attempts included,
    /// count against the run's budget.
    async fn prepare_context(&mut self, cancel: &CancellationToken) -> ContextPreparation;
}

/// The context of the error for an entry the session could not record.
pub(crate) const PERSIST_FAILED: &str = "cannot persist session entry";

/// Record the custom entries extensions asked for since the last turn (pi: `appendEntry`),
/// each announced as a `custom_entry` event. An error means the session could not record one.
pub(crate) async fn record_pending_entries(
    config: &LoopConfig,
    session: &mut SessionStore,
    target: EventTarget<'_>,
) -> Result<()> {
    for entry in config.queues.take_entries() {
        let entry_id = session
            .append_custom_entry(&entry.custom_type, entry.data)
            .await
            .context(PERSIST_FAILED)?;
        let event = AgentEvent::CustomEntry {
            entry_id,
            custom_type: entry.custom_type,
        };
        config.hooks.emit(event, target).await;
    }
    Ok(())
}

/// A session on its own: every step is recorded in the store, and the context is never
/// prepared, as no one summarizes it.
#[async_trait]
impl SessionHost for SessionStore {
    fn session(&mut self) -> &mut SessionStore {
        self
    }

    async fn prepare_context(&mut self, _cancel: &CancellationToken) -> ContextPreparation {
        ContextPreparation::ready()
    }
}

/// What preparing the context cost and how it ended. The loop knows neither agents nor
/// compaction policy; the cost counts against the run's budget whatever the result.
pub(crate) struct ContextPreparation {
    pub requests: u32,
    pub usage: Usage,
    /// `Ok` when the projection is the context to send, summarized or not; otherwise why the
    /// loop stops: preparation failed and left the context unchanged ([`Halt::Failed`]), or
    /// the session could not record it ([`Halt::Unrecorded`]).
    pub result: Result<(), Halt>,
}

impl ContextPreparation {
    /// No request was made; the context is ready as it is.
    pub(crate) fn ready() -> Self {
        Self {
            requests: 0,
            usage: Usage::default(),
            result: Ok(()),
        }
    }

    /// Preparation failed before any request was made.
    pub(crate) fn failed(error: String) -> Self {
        Self {
            requests: 0,
            usage: Usage::default(),
            result: Err(Halt::Failed(error)),
        }
    }
}
