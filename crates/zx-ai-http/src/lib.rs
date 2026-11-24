//! What the HTTP providers of the model layer share: the checks of their settings, the posting
//! of a request, the reading of the event stream it answers with, and the failures of each,
//! as every such provider sees them. What a protocol writes and what kind of failure a server's
//! status, error code or wording is stay with each provider (`zx-ai-openai`,
//! `zx-ai-anthropic`). The program's other HTTP clients send through proxies as these do
//! ([`client`]). Not part of the zx-agent API.
//!
//! - [`settings`]: the checks of where requests go, the headers they carry and the limits of
//!   their responses
//! - [`request`]: the API key asked for before each request, and the wait for the response
//! - [`stream`]: the data of each event of a response, read within the limits, and the end of
//!   the provider's own stream with the message made of the answer
//! - [`failure`]: why a request did not end with a complete answer, and what a failed response
//!   says besides its status
//! - `sse`: the parser of Server-Sent Events that [`stream`] reads with
//! - [`client`]: the proxies a client sends through, checked, for the providers and the
//!   program's other HTTP clients

pub mod client;
pub mod failure;
pub mod request;
pub mod settings;
mod sse;
pub mod stream;

use std::time::Duration;

pub use self::failure::Failure;
pub use self::sse::DEFAULT_MAX_EVENT_BYTES;

/// Longest wait for a connection to the server to be established. Fixed, unlike a provider's
/// idle timeout: how long a server takes to answer varies with the model and the request, how
/// long connecting takes does not.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
