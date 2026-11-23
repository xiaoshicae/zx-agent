//! Messages through the server and the backlog: the receipt each message carries until it is
//! handled, the task that completes handled messages in the backlog, and what an earlier
//! process left pending, read as the server starts.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{ServerEvent, ServerSink, Shared, timed};
use crate::backlog;
use crate::{Backlog, Inbound, MessageKey};

/// A message's place in the backlog, which the message carries until it is handled: written
/// into its session, answered without a run, or dropped on purpose. Dropping the receipt
/// completes the message ([`Backlog::complete`]), unless the server is stopping: what it has
/// not handled by then stays pending, for the next start.
pub(super) struct Receipt {
    /// Taken as the receipt is dropped.
    key: Option<MessageKey>,
    handled: mpsc::UnboundedSender<MessageKey>,
    stop: CancellationToken,
}

impl Drop for Receipt {
    fn drop(&mut self) {
        if self.stop.is_cancelled() {
            return;
        }
        if let Some(key) = self.key.take() {
            // Fails only once the completing task has failed, which is reported; the message
            // is then handled again at the next start.
            self.handled.send(key).ok();
        }
    }
}

/// Makes the receipts of the messages the router takes ([`Receipt`]).
pub(super) struct Receipts {
    handled: mpsc::UnboundedSender<MessageKey>,
    stop: CancellationToken,
}

impl Receipts {
    pub(super) fn receipt(&self, key: MessageKey) -> Receipt {
        Receipt {
            key: Some(key),
            handled: self.handled.clone(),
            stop: self.stop.clone(),
        }
    }

    /// The receipt of input that the backlog does not keep, such as a trigger's prompt:
    /// dropping it completes nothing.
    pub(super) fn unkept(&self) -> Receipt {
        Receipt {
            key: None,
            handled: self.handled.clone(),
            stop: self.stop.clone(),
        }
    }
}

/// Start the task that completes in `backlog` each message whose receipt is dropped, and
/// return the receipts' maker with it. The task ends once the maker and every receipt are
/// gone. Each call may take `timeout`; a failure is reported to `sink`, and that message is
/// handled again at the next start.
///
/// The receipts' queue is unbounded, as dropping a receipt cannot wait. It holds at most one
/// key per message kept and not yet completed, and channels keep one message at a time, no
/// faster than the backlog writes.
pub(super) fn complete_handled(
    backlog: Arc<dyn Backlog>,
    sink: Arc<dyn ServerSink>,
    timeout: Duration,
    stop: CancellationToken,
) -> (Receipts, JoinHandle<()>) {
    let (handled, mut keys) = mpsc::unbounded_channel::<MessageKey>();
    let task = tokio::spawn(async move {
        while let Some(key) = keys.recv().await {
            let act = format!("complete message {} in the backlog", key.message);
            if let Err(error) = timed(timeout, &act, backlog.complete(&key)).await {
                sink.emit(&ServerEvent::Failed { key: None, error });
            }
        }
    });
    (Receipts { handled, stop }, task)
}

/// What an earlier process left pending in `backlog`, in the order kept: each message with
/// the platform it arrived on. A record that is not a message is reported and stays pending;
/// nothing when the backlog cannot be read, which is reported.
pub(super) async fn left_pending(shared: &Shared, backlog: &dyn Backlog) -> Vec<(String, Inbound)> {
    // A failure was reported; what is pending stays for the next start.
    let pending = shared
        .bounded(None, "read the backlog", backlog.pending())
        .await
        .unwrap_or_default();
    let mut left = Vec::with_capacity(pending.len());
    for (key, record) in pending {
        match backlog::message(&key, &record) {
            Ok(event) => left.push((key.platform, event)),
            Err(error) => shared.failed(None, error),
        }
    }
    left
}
