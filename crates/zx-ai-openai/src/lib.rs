//! OpenAI-compatible Chat Completions provider with streaming (pi: packages/ai,
//! `openai-completions.ts` and `transform-messages.ts`).
//!
//! Works with OpenAI and the many servers that speak the same protocol
//! (vLLM, Ollama, llama.cpp server, LiteLLM, most hosted gateways). The only crate of the
//! model layer that needs an HTTP client: `zx-ai` holds the types and the `Provider`
//! contract. The `zx-agent` crate re-exports this crate as `zx_agent::ai::openai`.
//!
//! The settings are in `config.rs`, the request body in `request.rs`, the failures of a
//! request in `failure.rs` and the message built from the stream in `accumulate.rs`; this
//! module holds the provider and the HTTP flow of one request.

mod accumulate;
mod config;
mod failure;
mod request;
pub mod sse;

use std::time::Instant;

use futures::StreamExt;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use zx_ai::provider::{EventReceiver, EventSender, Provider, event_channel, send_event};
use zx_ai::{ProviderError, Request, StopReason, StreamEvent, duration_ms, now_ms};

use accumulate::Accumulator;
pub use config::{ApiKey, ApiKeySource, DEFAULT_MAX_RESPONSE_BYTES, OpenAiConfig, ReasoningFormat, SystemUpdates};
use failure::{
    Failure, answers_json, error_object, http_failure, json_body_failure, provider_failure, transport_failure,
};
pub use request::replay_body;
use sse::SseParser;

/// Streaming provider for the OpenAI Chat Completions protocol.
pub struct OpenAiCompatible {
    config: OpenAiConfig,
    client: reqwest::Client,
}

impl OpenAiCompatible {
    /// Check the settings and build the HTTP client. Errors: a byte budget or a timeout of
    /// zero, an extra field the provider sets itself, a header the client cannot send, and a
    /// client that cannot be built.
    pub fn new(config: OpenAiConfig) -> anyhow::Result<Self> {
        config.check()?;
        let mut builder = reqwest::Client::builder().connect_timeout(config.connect_timeout);
        if !config.use_system_proxy {
            builder = builder.no_proxy();
        }
        Ok(Self {
            client: builder.build()?,
            config,
        })
    }
}

impl Provider for OpenAiCompatible {
    fn name(&self) -> &str {
        &self.config.provider_name
    }

    fn request_options(&self) -> Value {
        request::request_options(&self.config)
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
    config: OpenAiConfig,
    request: Request,
    cancel: CancellationToken,
    mut tx: EventSender,
) {
    let started = Instant::now();
    let mut acc = Accumulator::new(&config.provider_name, &request.model, now_ms());
    let body = request::build_body(&request, &config);
    let url = format!("{}/chat/completions", config.base_url.trim_end_matches('/'));

    let outcome = tokio::select! {
        biased;
        () = cancel.cancelled() => Err(Failure::Aborted),
        result = stream_response(&client, &config, &url, &body, &mut acc, &mut tx) => result,
    };

    // Failures keep the content streamed so far.
    let mut message = match outcome {
        Ok(()) => acc
            .finish()
            .unwrap_or_else(|error| acc.failure(StopReason::Error, error, Some(ProviderError::Protocol))),
        Err(Failure::Aborted) => acc.failure(StopReason::Aborted, "Request was aborted", None),
        Err(Failure::Closed) => return,
        Err(Failure::Error { details, message }) => acc.failure(StopReason::Error, message, Some(details)),
    };
    message.duration_ms = Some(duration_ms(started.elapsed()));
    // Once the consumer closes, a terminal event has nobody left to report to.
    send_event(&mut tx, StreamEvent::ending(message)).await;
}

async fn stream_response(
    client: &reqwest::Client,
    config: &OpenAiConfig,
    url: &str,
    body: &Value,
    acc: &mut Accumulator,
    tx: &mut EventSender,
) -> Result<(), Failure> {
    let response = send_request(client, config, url, body).await?;
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

/// Post `body` and wait for the response headers within the idle timeout.
async fn send_request(
    client: &reqwest::Client,
    config: &OpenAiConfig,
    url: &str,
    body: &Value,
) -> Result<reqwest::Response, Failure> {
    let mut request = client.post(url).json(body);
    for (name, value) in &config.headers {
        request = request.header(name.as_str(), value.as_str());
    }
    // Asked for on every request, so a token that expired since the last one is replaced.
    let key = config
        .api_key
        .resolve()
        .map_err(|error| Failure::error(ProviderError::ApiKey, format!("cannot get an API key: {error}")))?;
    if let Some(key) = key {
        request = request.bearer_auth(key);
    }
    tokio::time::timeout(config.idle_timeout, request.send())
        .await
        .map_err(|_| {
            Failure::error(
                ProviderError::Timeout,
                format!(
                    "request timed out after {}s waiting for response headers",
                    config.idle_timeout.as_secs()
                ),
            )
        })?
        .map_err(|e| transport_failure("request failed", &e))
}

/// Read the event stream into `acc`, forwarding each delta, until `[DONE]` or its end.
async fn read_stream(
    response: reqwest::Response,
    config: &OpenAiConfig,
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
                ProviderError::Timeout,
                format!("stream stalled: no data for {}s", idle.as_secs()),
            )
        })?;
        let Some(chunk) = next else { break };
        let chunk = chunk.map_err(|e| transport_failure("stream read failed", &e))?;
        if chunk.len() > config.max_response_bytes.saturating_sub(received_bytes) {
            return Err(Failure::error(
                ProviderError::ResourceLimit,
                format!("stream response exceeds the {} byte limit", config.max_response_bytes),
            ));
        }
        received_bytes += chunk.len();
        let events = parser
            .push(&chunk)
            .map_err(|e| Failure::error(ProviderError::ResourceLimit, e))?;
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

/// Apply one SSE payload. Returns true at the `[DONE]` sentinel.
async fn handle_data(data: &str, acc: &mut Accumulator, tx: &mut EventSender) -> Result<bool, Failure> {
    let data = data.trim();
    if data == "[DONE]" {
        return Ok(true);
    }
    if data.is_empty() {
        return Ok(false);
    }
    let chunk: Value = serde_json::from_str(data)
        .map_err(|e| Failure::error(ProviderError::Protocol, format!("invalid stream chunk: {e}")))?;
    if let Some(error) = error_object(&chunk) {
        return Err(provider_failure(error));
    }
    for delta in acc.apply(&chunk) {
        if !send_event(tx, StreamEvent::Delta(delta)).await {
            return Err(Failure::Closed);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_error_chunk_ends_the_stream_as_a_provider_failure() {
        let mut acc = Accumulator::new("p", "m", 1);
        let (mut tx, _rx) = event_channel();
        let chunk = r#"{"error":{"message":"overloaded","code":"server_overloaded"}}"#;
        match handle_data(chunk, &mut acc, &mut tx).await {
            Err(Failure::Error { details, message }) => {
                assert_eq!(
                    details,
                    ProviderError::Provider {
                        code: Some("server_overloaded".into())
                    }
                );
                assert_eq!(message, "provider error: overloaded");
            }
            Err(Failure::Aborted | Failure::Closed) | Ok(_) => panic!("expected a provider error"),
        }
    }
}
