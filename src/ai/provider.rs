//! The provider contract.

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

use super::types::{Request, StreamEvent};

/// Receiving side of one streamed response.
///
/// Unbounded on purpose, like pi's `EventStream`: the producer never waits for the
/// consumer, so slow event handlers cannot stall reading from the network. One response
/// produces a bounded number of events, so the queue cannot grow without limit.
pub type EventReceiver = UnboundedReceiver<StreamEvent>;

/// A model provider.
///
/// Contract (same as pi's `StreamFn`, packages/agent/src/types.ts):
/// - `stream` returns immediately; setup and I/O run in a background task.
/// - It never panics and never returns an error. Every failure becomes a final
///   `StreamEvent::Error` whose message has `stop_reason` `Error` or `Aborted`,
///   so the agent loop needs no error handling around model calls.
/// - The stream ends with exactly one `Done` or `Error` event.
/// - Cancelling `cancel` ends the stream with `Aborted`, keeping content generated so far.
pub trait Provider: Send + Sync {
    /// Name recorded on assistant messages.
    fn name(&self) -> &str;
    /// Start one request and return its event stream.
    fn stream(&self, request: Request, cancel: CancellationToken) -> EventReceiver;
}

/// Send an event to the consumer of a stream.
///
/// Sending fails only when the receiver was dropped, which means nobody wants this response
/// anymore (for example the run ended). There is no one left to tell, so the error is ignored.
pub(crate) fn send_event(tx: &UnboundedSender<StreamEvent>, event: StreamEvent) {
    let _ = tx.unbounded_send(event);
}
