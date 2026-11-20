//! The provider contract, and the API key a provider authenticates with (pi:
//! packages/agent/src/types.ts).

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use futures::SinkExt;
use futures::channel::mpsc::{Receiver, Sender, channel};
use tokio_util::sync::CancellationToken;

use super::types::{Request, StreamEvent};

/// The error message of a response that ended because its request was cancelled
/// (`StopReason::Aborted`), as every provider writes it; pi's words.
pub const ABORTED_MESSAGE: &str = "Request was aborted";

/// Buffered events per response before the producer waits for its consumer.
pub(crate) const STREAM_EVENT_BUFFER: usize = 64;

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
/// Contract (same as pi's `StreamFn`):
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

/// Send with backpressure, for providers. False means the consumer closed; the producer must
/// stop. The send error itself is dropped: a closed receiver needs no further events.
pub async fn send_event(tx: &mut EventSender, event: StreamEvent) -> bool {
    tx.send(event).await.is_ok()
}

/// Where the API key of a request comes from, for the providers that authenticate with one.
#[derive(Clone, Default)]
pub enum ApiKey {
    /// No key: the request carries no credentials.
    #[default]
    None,
    /// The same key for every request.
    Fixed(String),
    /// A key asked for before every request, for credentials that expire (pi:
    /// `getApiKey`). A failure fails the request as
    /// [`ProviderErrorKind::ApiKey`](crate::ProviderErrorKind::ApiKey), which is not retried.
    Dynamic(Arc<dyn ApiKeySource>),
}

/// Supplies the key for the next request; see [`ApiKey::Dynamic`].
#[async_trait]
pub trait ApiKeySource: Send + Sync {
    /// The key to send, asked for before every request on the request's task, so a source
    /// may refresh an expired one over the network here. A provider bounds the wait, and
    /// stops waiting when the request is cancelled.
    async fn api_key(&self) -> Result<String, String>;
}

impl ApiKey {
    /// A fixed key; an empty one is no key.
    pub fn fixed(key: impl Into<String>) -> Self {
        let key = key.into();
        if key.is_empty() { Self::None } else { Self::Fixed(key) }
    }

    /// The key for the next request, or `None` to send no credentials. Fails when a dynamic
    /// source does, with its reason.
    pub async fn resolve(&self) -> Result<Option<String>, String> {
        match self {
            Self::None => Ok(None),
            Self::Fixed(key) => Ok(Some(key.clone())),
            Self::Dynamic(source) => source.api_key().await.map(|key| (!key.is_empty()).then_some(key)),
        }
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Debug output can reach logs and panic messages; the key must not.
        f.write_str(match self {
            Self::None => "none",
            Self::Fixed(_) => "[REDACTED]",
            Self::Dynamic(_) => "[dynamic]",
        })
    }
}

impl From<Option<String>> for ApiKey {
    fn from(key: Option<String>) -> Self {
        key.map_or(Self::None, Self::fixed)
    }
}

impl From<String> for ApiKey {
    fn from(key: String) -> Self {
        Self::fixed(key)
    }
}

impl From<&str> for ApiKey {
    fn from(key: &str) -> Self {
        Self::fixed(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_fixed_unless_it_is_empty() {
        assert!(matches!(ApiKey::fixed(""), ApiKey::None));
        assert!(matches!(ApiKey::from(Some("k".to_string())), ApiKey::Fixed(key) if key == "k"));
    }

    #[test]
    fn a_key_is_never_shown() {
        assert_eq!(format!("{:?}", ApiKey::fixed("sk-secret")), "[REDACTED]");
    }
}
