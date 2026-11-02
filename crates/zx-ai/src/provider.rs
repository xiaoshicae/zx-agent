//! The provider contract.

use futures::SinkExt;
use futures::channel::mpsc::{Receiver, Sender, channel};
use tokio_util::sync::CancellationToken;

use super::types::{Request, StreamEvent};

/// Buffered events per response before the producer waits for its consumer.
pub const STREAM_EVENT_BUFFER: usize = 64;

/// Receiving side of one streamed response, with bounded buffering and backpressure.
pub type EventReceiver = Receiver<StreamEvent>;

/// Sending side of one response. Producers await sends and stop when the receiver closes.
pub type EventSender = Sender<StreamEvent>;

/// Create the bounded event channel used by providers. A single sender can hold one
/// additional in-flight event beyond the shared buffer.
pub fn event_channel() -> (EventSender, EventReceiver) {
    channel(STREAM_EVENT_BUFFER)
}

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
    /// Non-secret options needed to replay wire payload conversion. Never include
    /// authentication, headers or credentials. Null leaves custom wire conversion opaque;
    /// the session still records the complete provider-neutral Request.
    fn request_options(&self) -> serde_json::Value {
        serde_json::Value::Null
    }
    /// Start one request and return its event stream.
    fn stream(&self, request: Request, cancel: CancellationToken) -> EventReceiver;
}

/// Send with backpressure. False means the consumer closed; the producer must stop.
pub(crate) async fn send_event(tx: &mut EventSender, event: StreamEvent) -> bool {
    tx.send(event).await.is_ok()
}
