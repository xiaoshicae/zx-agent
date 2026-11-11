//! What the loop needs from the session it runs in: the session, where it records each step
//! before acting on it and whose projection it sends the model, and a chance to prepare that
//! context before a request. The Runner's host is the only one, and adds summaries; the trait
//! keeps the loop from depending on the runner, the layer above it.

use anyhow::{Context, Result};
use async_trait::async_trait;

use super::Halt;
use crate::agent::{AgentEvent, EventTarget, MessageQueues, RunAccounting};
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
    /// records is in the projection afterwards. Returns how many model requests that took.
    /// The loop knows neither agents nor compaction policy: the host counts those requests
    /// and their usage into `accounting`, failed ones included, so that they count against
    /// the run's budget whatever the result.
    ///
    /// An error stops the loop: preparation failed and left the context unchanged
    /// ([`Halt::Failed`]), or the session could not record it ([`Halt::Unrecorded`]).
    async fn prepare_context(&mut self, accounting: &mut RunAccounting) -> Result<u32, Halt>;
}

/// The context of the error for an entry the session could not record.
pub(crate) const PERSIST_FAILED: &str = "cannot persist session entry";

/// Record the custom entries extensions queued since the last turn (pi: `appendEntry`), each
/// announced as a `custom_entry` event. An error means the session could not record one.
pub(crate) async fn record_pending_entries(
    queues: &MessageQueues,
    session: &mut SessionStore,
    target: EventTarget<'_>,
) -> Result<()> {
    for entry in queues.take_entries() {
        let entry_id = session
            .append_custom_entry(&entry.custom_type, entry.data)
            .await
            .context(PERSIST_FAILED)?;
        target
            .emit(AgentEvent::CustomEntry {
                entry_id,
                custom_type: entry.custom_type,
            })
            .await;
    }
    Ok(())
}
