//! Anthropic Messages API provider with streaming (pi: packages/ai,
//! `api/anthropic-messages.ts`).
//!
//! Speaks the Messages API of Anthropic and of the servers that offer the same protocol. A
//! provider crate of the model layer, with an HTTP client: `zx-ai` holds the types and the
//! `Provider` contract. The `zx-agent` crate re-exports this crate as
//! `zx_agent::ai::anthropic`.
//!
//! The settings are in `config.rs`, the request body in `request.rs`, the failures of a
//! request and what kind each is (the statuses and error types of the API) in `failure.rs`
//! and the message built from the stream's events in `accumulate.rs`; this module holds the
//! provider and the HTTP flow of one request, which reads the stream's events with
//! `zx_util::sse`.

mod accumulate;
mod config;
mod failure;
mod request;

use std::time::{Duration, Instant};

use futures::StreamExt;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use zx_ai::provider::{EventReceiver, EventSender, Provider, event_channel, send_event};
use zx_ai::{ProviderErrorKind, Request, StopReason, StreamEvent, duration_ms, now_ms};
use zx_util::sse::SseParser;

use accumulate::Accumulator;
pub use config::{AnthropicConfig, BodyOptions, DEFAULT_API_VERSION, SystemUpdates};
use config::{KEY_HEADER, VERSION_HEADER};
use failure::{Failure, answers_json, http_failure, json_body_failure, transport_failure};

/// Longest wait for a connection to the server to be established. Fixed, unlike
/// [`AnthropicConfig::idle_timeout`]: how long a server takes to answer varies with the model
/// and the request, how long connecting takes does not.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Streaming provider for the Anthropic Messages API.
pub struct AnthropicMessages {
    config: AnthropicConfig,
    client: reqwest::Client,
}

impl AnthropicMessages {
    /// Check the settings and build the HTTP client. Errors: an address requests cannot go
    /// to, a byte budget or a timeout of zero, an extra field the provider sets itself, a
    /// header the client cannot send, and a client that cannot be built.
    pub fn new(config: AnthropicConfig) -> anyhow::Result<Self> {
        config.check()?;
        let mut builder = reqwest::Client::builder().connect_timeout(CONNECT_TIMEOUT);
        if !config.use_system_proxy {
            builder = builder.no_proxy();
        }
        Ok(Self {
            client: builder.build()?,
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
    let mut post = Post {
        client: &client,
        config: &config,
        url: format!("{}/v1/messages", config.base_url.trim_end_matches('/')),
        body: request::build_body(&request, &config.body, &config.provider_name),
    };

    let outcome = tokio::select! {
        biased;
        () = cancel.cancelled() => Err(Failure::Aborted),
        result = stream_response(&mut post, &mut acc, &mut tx) => result,
    };

    // Failures keep the content streamed so far.
    let mut message = match outcome {
        Ok(()) => acc
            .finish()
            .unwrap_or_else(|error| acc.failure(StopReason::Error, error, Some(ProviderErrorKind::Protocol.into()))),
        Err(Failure::Aborted) => acc.failure(StopReason::Aborted, "Request was aborted", None),
        Err(Failure::Closed) => return,
        Err(Failure::Error { details, message }) => acc.failure(StopReason::Error, message, Some(details)),
    };
    message.duration_ms = Some(duration_ms(started.elapsed()));
    // Once the consumer closes, a terminal event has nobody left to report to.
    send_event(&mut tx, StreamEvent::ending(message)).await;
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
async fn stream_response(post: &mut Post<'_>, acc: &mut Accumulator, tx: &mut EventSender) -> Result<(), Failure> {
    let config = post.config;
    let response = answered(post).await?;
    if answers_json(&response) {
        return Err(json_body_failure(response, config).await);
    }
    if !send_event(tx, StreamEvent::Start).await {
        return Err(Failure::Closed);
    }
    read_stream(response, config, acc, tx).await
}

/// Post the request and return the response once its status is a success. A request the API
/// refused because a thinking block is bound to a different conversation, after a compaction
/// or a changed system prompt, goes once more without the thinking from the message the API
/// named on, as the API's error advises; sending the same body again could never succeed.
async fn answered(post: &mut Post<'_>) -> Result<reqwest::Response, Failure> {
    let mut retried = false;
    loop {
        let response = send_request(post).await?;
        if response.status().is_success() {
            return Ok(response);
        }
        let failure = http_failure(response, post.config).await;
        match failure.thinking_bound_elsewhere() {
            Some(from) if !retried => {
                request::drop_thinking_from(&mut post.body, from);
                retried = true;
            }
            _ => return Err(failure),
        }
    }
}

/// Post the request and wait for the response headers within the idle timeout.
async fn send_request(post: &Post<'_>) -> Result<reqwest::Response, Failure> {
    let config = post.config;
    let mut request = post.client.post(&post.url).json(&post.body);
    if !config.names_version() {
        request = request.header(VERSION_HEADER, config::DEFAULT_API_VERSION);
    }
    for (name, value) in &config.headers {
        request = request.header(name.trim(), value.as_str());
    }
    // Asked for on every request, so a key that expired since the last one is replaced.
    let key = tokio::time::timeout(config.idle_timeout, config.api_key.resolve())
        .await
        .map_err(|_| {
            Failure::error(
                ProviderErrorKind::ApiKey,
                format!(
                    "the API key source did not answer within {}s",
                    config.idle_timeout.as_secs()
                ),
            )
        })?
        .map_err(|error| Failure::error(ProviderErrorKind::ApiKey, format!("cannot get an API key: {error}")))?;
    if let Some(key) = key {
        request = request.header(KEY_HEADER, key);
    }
    tokio::time::timeout(config.idle_timeout, request.send())
        .await
        .map_err(|_| {
            Failure::error(
                ProviderErrorKind::Timeout,
                format!(
                    "request timed out after {}s waiting for response headers",
                    config.idle_timeout.as_secs()
                ),
            )
        })?
        .map_err(|e| transport_failure("request failed", &e))
}

/// Read the event stream into `acc`, forwarding each delta, until `message_stop` or its end.
/// The loop of zx-ai-openai's: the providers share no crate that has an HTTP client.
async fn read_stream(
    response: reqwest::Response,
    config: &AnthropicConfig,
    acc: &mut Accumulator,
    tx: &mut EventSender,
) -> Result<(), Failure> {
    let idle = config.idle_timeout;
    let mut parser = SseParser::new(config.max_sse_event_bytes);
    let mut body = response.bytes_stream();
    let mut received_bytes = 0;
    loop {
        let next = tokio::time::timeout(idle, body.next()).await.map_err(|_| {
            Failure::error(
                ProviderErrorKind::Timeout,
                format!("stream stalled: no data for {}s", idle.as_secs()),
            )
        })?;
        let Some(chunk) = next else { break };
        let chunk = chunk.map_err(|e| transport_failure("stream read failed", &e))?;
        if chunk.len() > config.max_response_bytes.saturating_sub(received_bytes) {
            return Err(Failure::error(
                ProviderErrorKind::ResourceLimit,
                format!("stream response exceeds the {} byte limit", config.max_response_bytes),
            ));
        }
        received_bytes += chunk.len();
        let events = parser
            .push(&chunk)
            .map_err(|e| Failure::error(ProviderErrorKind::ResourceLimit, e))?;
        for data in events {
            if handle_data(&data, acc, tx).await? {
                return Ok(());
            }
        }
    }
    if let Some(data) = parser.finish() {
        handle_data(&data, acc, tx).await?;
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
        .map_err(|e| Failure::error(ProviderErrorKind::Protocol, format!("invalid stream event: {e}")))?;
    for delta in acc.apply(&event)? {
        if !send_event(tx, StreamEvent::Delta(delta)).await {
            return Err(Failure::Closed);
        }
    }
    Ok(event["type"] == "message_stop")
}
