//! OpenAI-compatible Chat Completions provider with streaming (pi: packages/ai,
//! `openai-completions.ts` and `transform-messages.ts`).
//!
//! Works with OpenAI and the many servers that speak the same protocol
//! (vLLM, Ollama, llama.cpp server, LiteLLM, most hosted gateways). A provider crate of the
//! model layer, with an HTTP client: `zx-ai` holds the types and the `Provider` contract. The
//! `zx-agent` crate re-exports this crate as `zx_agent::ai::openai`.
//!
//! This module holds the provider and the HTTP flow of one request, with what the HTTP
//! providers share (`zx-ai-http`):
//! - `config`: the settings and their checks
//! - `request`: the request body, the options recorded with it, and the body rebuilt from them
//! - `failure`: the failures of a request, and what kind each is by the error codes and
//!   wording of OpenAI-compatible services
//! - `accumulate`: the message built from the stream's chunks

mod accumulate;
mod config;
mod failure;
mod request;

use std::time::Instant;

use serde_json::Value;
use tokio_util::sync::CancellationToken;
use zx_ai::{EventReceiver, EventSender, Provider, event_channel, send_event};
use zx_ai::{ProviderErrorKind, Request, StreamEvent, now_ms};
use zx_ai_http::failure::answers_json;
use zx_ai_http::request::{api_key, send};
use zx_ai_http::stream::{Events, end};
use zx_ai_http::{CONNECT_TIMEOUT, Failure};

use self::accumulate::Accumulator;
pub use self::config::{BodyOptions, MaxTokensField, OpenAiConfig, ReasoningFormat, SystemUpdates};
use self::failure::{error_object, http_failure, json_body_failure, provider_failure};
pub use self::request::replay_body;

/// Streaming provider for the OpenAI Chat Completions protocol.
pub struct OpenAiCompatible {
    config: OpenAiConfig,
    client: reqwest::Client,
}

impl OpenAiCompatible {
    /// Check the settings and build the HTTP client. Errors: an address requests cannot go
    /// to, a byte budget or a timeout of zero, an extra field the provider sets itself, a
    /// header the client cannot send, and a client that cannot be built.
    pub fn new(config: OpenAiConfig) -> anyhow::Result<Self> {
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

impl Provider for OpenAiCompatible {
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
    config: OpenAiConfig,
    request: Request,
    cancel: CancellationToken,
    mut tx: EventSender,
) {
    let started = Instant::now();
    let mut acc = Accumulator::new(&config.provider_name, &request.model, now_ms());
    let post = Post {
        client: &client,
        config: &config,
        url: format!("{}/chat/completions", config.base_url.trim_end_matches('/')),
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
    config: &'a OpenAiConfig,
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

/// Post the request, with the API key as a bearer token, and wait for the response headers
/// within the idle timeout.
async fn send_request(post: &Post<'_>) -> Result<reqwest::Response, Failure> {
    let config = post.config;
    let mut request = post.client.post(&post.url).json(&post.body);
    for (name, value) in &config.headers {
        request = request.header(name.as_str(), value.as_str());
    }
    if let Some(key) = api_key(&config.api_key, config.idle_timeout).await? {
        request = request.bearer_auth(key);
    }
    send(request, config.idle_timeout).await
}

/// Read the event stream into `acc`, forwarding each delta, until `[DONE]` or its end.
async fn read_stream(
    response: reqwest::Response,
    config: &OpenAiConfig,
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
        .map_err(|error| Failure::error(ProviderErrorKind::Protocol, format!("invalid stream chunk: {error}")))?;
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
        let Failure::Error {
            provider_error,
            message,
        } = handle_data(chunk, &mut acc, &mut tx).await.unwrap_err()
        else {
            panic!("expected a provider error");
        };
        // The code is not a known one, so the wording says what kind of failure it is.
        assert_eq!(
            provider_error,
            zx_ai::ProviderError {
                kind: ProviderErrorKind::Unavailable,
                status: None,
                code: Some("server_overloaded".into()),
                retry_after_ms: None,
            }
        );
        assert_eq!(message, "provider error: overloaded");
    }
}
