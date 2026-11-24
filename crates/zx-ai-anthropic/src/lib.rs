//! Anthropic Messages API provider with streaming (pi: packages/ai,
//! `api/anthropic-messages.ts`).
//!
//! Speaks the Messages API of Anthropic and of the servers that offer the same protocol. A
//! provider crate of the model layer, with an HTTP client: `zx-ai` holds the types and the
//! `Provider` contract. The `zx-agent` crate re-exports this crate as
//! `zx_agent::ai::anthropic`.
//!
//! This module holds the provider and the HTTP flow of one request, with what the HTTP
//! providers share (`zx-ai-http`):
//! - `config`: the settings and their checks
//! - `request`: the request body, the options recorded with it, and the body rebuilt from them
//! - `failure`: the failures of a request, and what kind each is by the statuses and error
//!   types of the API
//! - `accumulate`: the message built from the stream's events

mod accumulate;
mod config;
mod failure;
mod request;

use std::time::Instant;

use serde_json::Value;
use tokio_util::sync::CancellationToken;
use zx_ai::{EventReceiver, EventSender, Provider, event_channel, send_event};
use zx_ai::{ProviderErrorKind, Request, StreamEvent, now_ms};
use zx_ai_http::client::with_proxy;
use zx_ai_http::failure::answers_json;
use zx_ai_http::request::{api_key, send};
use zx_ai_http::stream::{Events, end};
use zx_ai_http::{CONNECT_TIMEOUT, Failure};

use self::accumulate::Accumulator;
use self::config::VERSION_HEADER;
pub use self::config::{AnthropicConfig, ApiKeyHeader, BodyOptions, DEFAULT_API_VERSION, SystemUpdates};
use self::failure::{http_failure, json_body_failure};
pub use self::request::replay_body;

/// Streaming provider for the Anthropic Messages API.
pub struct AnthropicMessages {
    config: AnthropicConfig,
    client: reqwest::Client,
}

impl AnthropicMessages {
    /// Check the settings and build the HTTP client. Errors: an address requests cannot go
    /// to, a byte budget or a timeout of zero, an extra field the provider sets itself, a
    /// header the client cannot send, a proxy that is not the `http://` URL of one, and a
    /// client that cannot be built.
    pub fn new(config: AnthropicConfig) -> anyhow::Result<Self> {
        config.check()?;
        let builder = reqwest::Client::builder().connect_timeout(CONNECT_TIMEOUT);
        Ok(Self {
            client: with_proxy(builder, &config.proxy)
                .map_err(anyhow::Error::msg)?
                .build()?,
            config,
        })
    }
}

impl Provider for AnthropicMessages {
    fn name(&self) -> &str {
        &self.config.provider_name
    }

    fn request_options(&self) -> Value {
        request::request_options(&self.config.body)
    }

    fn stream(&self, request: Request, cancel: CancellationToken) -> EventReceiver {
        let (tx, rx) = event_channel();
        let client = self.client.clone();
        let config = self.config.clone();
        // Not awaited: the task ends with the response, at its final event, when the
        // receiver is dropped or when `cancel` fires (the `Provider` contract).
        tokio::spawn(async move { run(client, config, request, cancel, tx).await });
        rx
    }
}

/// One request: send it, stream the answer into the message, and end the stream with that
/// message, whatever happened.
async fn run(
    client: reqwest::Client,
    config: AnthropicConfig,
    request: Request,
    cancel: CancellationToken,
    mut tx: EventSender,
) {
    let started = Instant::now();
    let mut acc = Accumulator::new(&config.provider_name, &request.model, now_ms());
    let post = Post {
        client: &client,
        config: &config,
        url: format!("{}/v1/messages", config.base_url.trim_end_matches('/')),
        body: request::build_body(&request, &config.body, &config.provider_name),
    };

    let outcome = tokio::select! {
        biased;
        () = cancel.cancelled() => Err(Failure::Aborted),
        result = stream_response(&post, &mut acc, &mut tx) => result,
    };
    end(outcome, &acc, started, &mut tx).await;
}

/// A request ready to post: the client, the provider's settings, where it goes and its body.
struct Post<'a> {
    client: &'a reqwest::Client,
    config: &'a AnthropicConfig,
    url: String,
    body: Value,
}

/// Post the request, check its answer, and stream the answer into `acc`, forwarding each
/// delta through `tx`.
async fn stream_response(post: &Post<'_>, acc: &mut Accumulator, tx: &mut EventSender) -> Result<(), Failure> {
    let config = post.config;
    let response = send_request(post).await?;
    if !response.status().is_success() {
        return Err(http_failure(response, config).await);
    }
    if answers_json(&response) {
        return Err(json_body_failure(response, config).await);
    }
    if !send_event(tx, StreamEvent::Start).await {
        return Err(Failure::Closed);
    }
    read_stream(response, config, acc, tx).await
}

/// Post the request, with the API version and the API key in their headers, and wait for the
/// response headers within the idle timeout.
async fn send_request(post: &Post<'_>) -> Result<reqwest::Response, Failure> {
    let config = post.config;
    let mut request = post.client.post(&post.url).json(&post.body);
    if !config.names_version() {
        request = request.header(VERSION_HEADER, DEFAULT_API_VERSION);
    }
    for (name, value) in &config.headers {
        request = request.header(name.as_str(), value.as_str());
    }
    if let Some(key) = api_key(&config.api_key, config.idle_timeout).await? {
        let value = match config.api_key_header {
            ApiKeyHeader::XApiKey => key,
            ApiKeyHeader::Authorization => format!("Bearer {key}"),
        };
        let mut value =
            reqwest::header::HeaderValue::from_str(&value).expect("`api_key` gives keys a header can carry");
        // Kept out of debug output, as `bearer_auth` keeps the OpenAI-compatible provider's.
        value.set_sensitive(true);
        request = request.header(config.api_key_header.name(), value);
    }
    send(request, config.idle_timeout).await
}

/// Read the event stream into `acc`, forwarding each delta, until `message_stop` or its end.
/// The loop of zx-ai-openai's, with this provider's events: sharing it would take a callback.
async fn read_stream(
    response: reqwest::Response,
    config: &AnthropicConfig,
    acc: &mut Accumulator,
    tx: &mut EventSender,
) -> Result<(), Failure> {
    let mut events = Events::new(
        response,
        config.idle_timeout,
        config.max_response_bytes,
        config.max_sse_event_bytes,
    );
    while let Some(data) = events.next().await? {
        if handle_data(&data, acc, tx).await? {
            return Ok(());
        }
    }
    Ok(())
}

/// Apply one SSE payload. Returns true at `message_stop`, the end of the response, or at the
/// `[DONE]` some servers of the same protocol end with.
async fn handle_data(data: &str, acc: &mut Accumulator, tx: &mut EventSender) -> Result<bool, Failure> {
    let data = data.trim();
    if data == "[DONE]" {
        return Ok(true);
    }
    if data.is_empty() {
        return Ok(false);
    }
    let event: Value = serde_json::from_str(data)
        .map_err(|error| Failure::error(ProviderErrorKind::Protocol, format!("invalid stream event: {error}")))?;
    for delta in acc.apply(&event)? {
        if !send_event(tx, StreamEvent::Delta(delta)).await {
            return Err(Failure::Closed);
        }
    }
    Ok(event["type"] == "message_stop")
}
