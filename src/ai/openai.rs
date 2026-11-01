//! OpenAI-compatible Chat Completions provider with streaming.
//!
//! Works with OpenAI and the many servers that speak the same protocol
//! (vLLM, Ollama, llama.cpp server, LiteLLM, most hosted gateways).

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

use futures::StreamExt;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::provider::{EventReceiver, EventSender, Provider, event_channel, send_event};
use super::sse::{DEFAULT_MAX_EVENT_BYTES, SseParser};
use super::system_prompt::SystemPrompt;
use super::types::{
    AssistantMessage, ContentBlock, Delta, Message, ProviderError, Request, StopReason, StreamEvent, Usage,
    duration_ms, now_ms,
};

/// Default maximum raw SSE response bytes per model request, including reasoning.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
/// Error bodies only add diagnostic detail; keep their memory use independent of the server.
const MAX_ERROR_BODY_BYTES: usize = 8 * 1024;

/// Response fields that carry reasoning text: `reasoning_content` (DeepSeek, vLLM) or
/// `reasoning` (OpenRouter, Ollama). The first one present wins; replay uses the same name.
const REASONING_FIELDS: [&str; 2] = ["reasoning_content", "reasoning"];

/// Connection settings for an OpenAI-compatible server. Debug output redacts the API key.
#[derive(Clone)]
pub struct OpenAiConfig {
    /// Base URL without `/chat/completions`, e.g. `https://api.openai.com/v1`.
    pub base_url: String,
    /// Sent as a bearer token when present. Local servers often need none.
    pub api_key: Option<String>,
    /// Name recorded on assistant messages.
    pub provider_name: String,
    /// Longest wait for response headers and between two stream chunks.
    pub idle_timeout: Duration,
    pub connect_timeout: Duration,
    /// Honor HTTP(S)_PROXY environment variables.
    pub use_system_proxy: bool,
    /// How system prompt changes after the first one reach the model.
    pub system_updates: SystemUpdates,
    /// Maximum raw streaming response bytes per request.
    pub max_response_bytes: usize,
    /// Maximum bytes in a single SSE event, including framing.
    pub max_sse_event_bytes: usize,
}

impl fmt::Debug for OpenAiConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Debug output can reach logs and panic messages; the key must not.
        f.debug_struct("OpenAiConfig")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field("provider_name", &self.provider_name)
            .field("idle_timeout", &self.idle_timeout)
            .field("connect_timeout", &self.connect_timeout)
            .field("use_system_proxy", &self.use_system_proxy)
            .field("system_updates", &self.system_updates)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("max_sse_event_bytes", &self.max_sse_event_bytes)
            .finish()
    }
}

/// How system prompt changes after the first one are sent (pi: the
/// `supportsMidConvoSystemMessages` compat flag).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SystemUpdates {
    /// Fold versions into one leading system message, for servers that reject mid-conversation
    /// system messages. Prompt changes can reduce reuse of the previous request prefix.
    /// pi's default for OpenAI-compatible servers.
    #[default]
    Collapse,
    /// Send changes at their original positions. With other request settings unchanged,
    /// earlier messages remain a prefix. The server must accept mid-conversation system messages.
    Inline,
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: None,
            provider_name: "openai-compatible".to_string(),
            idle_timeout: Duration::from_secs(120),
            connect_timeout: Duration::from_secs(15),
            use_system_proxy: true,
            system_updates: SystemUpdates::default(),
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            max_sse_event_bytes: DEFAULT_MAX_EVENT_BYTES,
        }
    }
}

/// Streaming provider for the OpenAI Chat Completions protocol.
pub struct OpenAiCompatible {
    config: OpenAiConfig,
    client: reqwest::Client,
}

impl OpenAiCompatible {
    /// Validate byte budgets and build the HTTP client.
    pub fn new(config: OpenAiConfig) -> anyhow::Result<Self> {
        anyhow::ensure!(
            config.max_response_bytes > 0,
            "max_response_bytes must be greater than zero"
        );
        anyhow::ensure!(
            config.max_sse_event_bytes > 0,
            "max_sse_event_bytes must be greater than zero"
        );
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
        json!({"format":"openai-chat-completions","version":1,"systemUpdates":match self.config.system_updates { SystemUpdates::Collapse => "collapse", SystemUpdates::Inline => "inline" }})
    }

    fn stream(&self, request: Request, cancel: CancellationToken) -> EventReceiver {
        let (tx, rx) = event_channel();
        let client = self.client.clone();
        let config = self.config.clone();
        tokio::spawn(async move { run(client, config, request, cancel, tx).await });
        rx
    }
}

enum Failure {
    Aborted,
    Closed,
    Error { details: ProviderError, message: String },
}

impl Failure {
    fn error(details: ProviderError, message: impl Into<String>) -> Self {
        Self::Error {
            details,
            message: message.into(),
        }
    }
}

async fn run(
    client: reqwest::Client,
    config: OpenAiConfig,
    request: Request,
    cancel: CancellationToken,
    mut tx: EventSender,
) {
    let started = Instant::now();
    let mut acc = Accumulator::new(&config.provider_name, &request.model, now_ms());
    let body = build_body(&request, &config);
    let url = format!("{}/chat/completions", config.base_url.trim_end_matches('/'));

    let outcome = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(Failure::Aborted),
        result = stream_response(&client, &config, &url, &body, &mut acc, &mut tx) => result,
    };

    // Failures keep the content streamed so far, like pi's providers.
    let mut message = match outcome {
        Ok(()) => acc.finish().unwrap_or_else(|error| {
            let mut message = acc.failure(StopReason::Error, error);
            message.provider_error = Some(ProviderError::Protocol);
            message
        }),
        Err(Failure::Aborted) => acc.failure(StopReason::Aborted, "Request was aborted"),
        Err(Failure::Closed) => return,
        Err(Failure::Error { details, message }) => {
            let mut failed = acc.failure(StopReason::Error, message);
            failed.provider_error = Some(details);
            failed
        }
    };
    message.duration_ms = Some(duration_ms(started.elapsed()));
    let event = match message.stop_reason {
        StopReason::Error | StopReason::Aborted => StreamEvent::Error(message),
        StopReason::Stop | StopReason::Length | StopReason::ToolUse => StreamEvent::Done(message),
    };
    // Once the consumer closes, a terminal event has nobody left to report to.
    send_event(&mut tx, event).await;
}

async fn stream_response(
    client: &reqwest::Client,
    config: &OpenAiConfig,
    url: &str,
    body: &Value,
    acc: &mut Accumulator,
    tx: &mut EventSender,
) -> Result<(), Failure> {
    let idle = config.idle_timeout;
    let mut request = client.post(url).json(body);
    if let Some(key) = config.api_key.as_deref().filter(|key| !key.is_empty()) {
        request = request.bearer_auth(key);
    }
    let response = tokio::time::timeout(idle, request.send())
        .await
        .map_err(|_| {
            Failure::error(
                ProviderError::Timeout,
                format!(
                    "request timed out after {}s waiting for response headers",
                    idle.as_secs()
                ),
            )
        })?
        .map_err(|e| transport_failure("request failed", &e))?;

    let status = response.status();
    if !status.is_success() {
        // Only numeric delay-seconds hints are used. Invalid/HTTP-date hints are optional
        // and fall back to local backoff; they do not invalidate the HTTP error.
        let retry_after_ms = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(|seconds| seconds.saturating_mul(1000));
        // Body errors only lose diagnostics; the HTTP status remains authoritative.
        let bytes = match tokio::time::timeout(
            idle,
            read_error_body(response, MAX_ERROR_BODY_BYTES.min(config.max_response_bytes)),
        )
        .await
        {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(_)) | Err(_) => Vec::new(),
        };
        let code = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|body| body.get("error").and_then(error_code));
        let text = String::from_utf8_lossy(&bytes);
        return Err(Failure::error(
            ProviderError::Http {
                status: status.as_u16(),
                code,
                retry_after_ms,
            },
            format!("HTTP {}: {}", status.as_u16(), clip(text.trim(), 2000)),
        ));
    }

    if !send_event(tx, StreamEvent::Start).await {
        return Err(Failure::Closed);
    }
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
                format!("stream response exceeds the {} byte limit", config.max_response_bytes,),
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
    if let Some(error) = chunk.get("error").filter(|error| !error.is_null()) {
        let text = error
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| error.to_string());
        return Err(Failure::error(
            ProviderError::Provider {
                code: error_code(error),
            },
            format!("provider error: {text}"),
        ));
    }
    for delta in acc
        .apply(&chunk)
        .map_err(|e| Failure::error(ProviderError::Protocol, e))?
    {
        if !send_event(tx, StreamEvent::Delta(delta)).await {
            return Err(Failure::Closed);
        }
    }
    Ok(false)
}

fn error_code(error: &Value) -> Option<String> {
    error
        .get("code")
        .and_then(Value::as_str)
        .or_else(|| error.get("type").and_then(Value::as_str))
        .map(str::to_string)
}

fn transport_failure(operation: &str, error: &reqwest::Error) -> Failure {
    let details = if error.is_timeout() {
        ProviderError::Timeout
    } else {
        ProviderError::Transport
    };
    Failure::error(details, format!("{operation}: {}", with_causes(error)))
}

async fn read_error_body(response: reqwest::Response, max_bytes: usize) -> Result<Vec<u8>, reqwest::Error> {
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let keep = chunk.len().min(max_bytes.saturating_sub(bytes.len()));
        bytes.extend_from_slice(&chunk[..keep]);
        if bytes.len() == max_bytes {
            break;
        }
    }
    Ok(bytes)
}

/// `error` followed by its causes. reqwest's own message omits the cause, such as
/// "Connection refused", which is what an unattended run needs for diagnosis. A cause whose
/// text is already included is skipped.
fn with_causes(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.contains(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

fn clip(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((index, _)) => format!("{}...", &text[..index]),
        None => text.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Request body
// ---------------------------------------------------------------------------

/// The JSON body of a streaming Chat Completions request.
pub fn build_body(request: &Request, config: &OpenAiConfig) -> Value {
    let messages = convert_messages(&request.messages, config, &request.model);

    let mut body = json!({
        "model": request.model,
        "messages": messages,
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.parameters,
                        }
                    })
                })
                .collect(),
        );
    }
    if let Some(max_tokens) = request.max_tokens {
        body["max_tokens"] = json!(max_tokens);
    }
    body
}

/// Rebuild a Chat Completions JSON body from a persisted RequestConfig and Request.
/// Credentials, URLs and HTTP headers are intentionally absent. Rejects unknown option
/// formats/versions rather than silently applying a different conversion policy.
pub fn replay_body(request: &Request, provider: &str, options: &Value) -> anyhow::Result<Value> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields, rename_all = "camelCase")]
    struct Options {
        format: String,
        version: u32,
        system_updates: String,
    }
    let options: Options = serde_json::from_value(options.clone())?;
    anyhow::ensure!(
        options.format == "openai-chat-completions" && options.version == 1,
        "unsupported request conversion format/version"
    );
    let system_updates = match options.system_updates.as_str() {
        "collapse" => SystemUpdates::Collapse,
        "inline" => SystemUpdates::Inline,
        value => anyhow::bail!("unknown system update mode {value:?}"),
    };
    Ok(build_body(
        request,
        &OpenAiConfig {
            provider_name: provider.into(),
            system_updates,
            ..OpenAiConfig::default()
        },
    ))
}

/// Convert the transcript, keeping tool calls and results paired.
///
/// Providers reject a tool call without a result and a result without its call. A run
/// cancelled between them leaves such gaps, so missing results get a placeholder and
/// unmatched results are dropped (pi: packages/ai/src/api/transform-messages.ts).
///
/// Reasoning is sent back only to the provider (`config.provider_name`) and model that
/// produced it. System messages follow `config.system_updates`.
fn convert_messages(messages: &[Message], config: &OpenAiConfig, model: &str) -> Vec<Value> {
    let provider = config.provider_name.as_str();
    let mut out = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    let inline = config.system_updates == SystemUpdates::Inline;
    if !inline {
        let prompt = SystemPrompt::from_transcript(messages);
        if !prompt.is_empty() {
            out.push(json!({ "role": "system", "content": prompt.render() }));
        }
    }
    // Inline mode: the prompt so far; the first system message is sent in full.
    let mut prompt = SystemPrompt::default();
    for message in messages {
        match message {
            Message::System(_) if !inline => {}
            Message::System(system) => {
                flush_missing_results(&mut out, &mut pending);
                let first = prompt.is_empty();
                prompt.apply(&system.sections);
                let text = if first { prompt.render() } else { system.render_update() };
                if !text.is_empty() {
                    out.push(json!({ "role": "system", "content": text }));
                }
            }
            Message::ToolResult(result) => {
                let Some(pos) = pending.iter().position(|id| *id == result.tool_call_id) else {
                    continue;
                };
                pending.remove(pos);
                let content = if result.content.is_empty() {
                    "(no output)"
                } else {
                    result.content.as_str()
                };
                out.push(json!({ "role": "tool", "tool_call_id": result.tool_call_id, "content": content }));
            }
            Message::User { content, .. } => {
                flush_missing_results(&mut out, &mut pending);
                out.push(json!({ "role": "user", "content": content }));
            }
            Message::Assistant(assistant) => {
                flush_missing_results(&mut out, &mut pending);
                let text = assistant.text();
                let calls = assistant.tool_calls();
                if text.is_empty() && calls.is_empty() {
                    // An empty failed or aborted attempt carries nothing for the model.
                    continue;
                }
                let mut entry = json!({
                    "role": "assistant",
                    "content": if text.is_empty() { Value::Null } else { Value::String(text) },
                });
                if let Some((field, reasoning)) = replayed_reasoning(assistant, provider, model) {
                    entry[field] = Value::String(reasoning);
                }
                if !calls.is_empty() {
                    entry["tool_calls"] = Value::Array(
                        calls
                            .iter()
                            .map(|call| {
                                json!({
                                    "id": call.id,
                                    "type": "function",
                                    "function": { "name": call.name, "arguments": call.arguments.to_string() },
                                })
                            })
                            .collect(),
                    );
                    pending = calls.into_iter().map(|call| call.id).collect();
                }
                out.push(entry);
            }
        }
    }
    flush_missing_results(&mut out, &mut pending);
    fill_missing_reasoning(&mut out);
    out
}

/// The reasoning to send back with an assistant message: the field it arrived in and its
/// text. Reasoning is meaningful only to the model that produced it (pi: transform-messages.ts).
fn replayed_reasoning(assistant: &AssistantMessage, provider: &str, model: &str) -> Option<(&'static str, String)> {
    if assistant.provider != provider || assistant.model != model {
        return None;
    }
    let mut blocks = assistant
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Thinking { thinking, signature } if !thinking.trim().is_empty() => {
                Some((signature.as_deref(), thinking.as_str()))
            }
            _ => None,
        })
        .peekable();
    let (signature, _) = *blocks.peek()?;
    let field = REASONING_FIELDS.into_iter().find(|field| signature == Some(*field))?;
    let text: Vec<&str> = blocks.map(|(_, text)| text).collect();
    Some((field, text.join("\n")))
}

/// Thinking-mode servers reject a history in which only some assistant messages carry
/// reasoning. DeepSeek answers "The `reasoning_content` in the thinking mode must be passed
/// back to the API." (pi #3668), so once one message carries the field, the others get it
/// empty. pi adds it to every message of a DeepSeek reasoning model, which needs a model
/// catalog; a live DeepSeek request whose history had no reasoning at all succeeded.
fn fill_missing_reasoning(messages: &mut [Value]) {
    const FIELD: &str = "reasoning_content";
    if !messages.iter().any(|message| message.get(FIELD).is_some()) {
        return;
    }
    for message in messages.iter_mut().filter(|message| message["role"] == "assistant") {
        if message.get(FIELD).is_none() {
            message[FIELD] = json!("");
        }
    }
}

fn flush_missing_results(out: &mut Vec<Value>, pending: &mut Vec<String>) {
    for id in pending.drain(..) {
        out.push(json!({ "role": "tool", "tool_call_id": id, "content": "No result provided" }));
    }
}

// ---------------------------------------------------------------------------
// Stream accumulation
// ---------------------------------------------------------------------------

enum Block {
    Text(String),
    /// Reasoning text and the response field it arrived in.
    Thinking {
        text: String,
        field: &'static str,
    },
    Tool {
        id: String,
        name: String,
        args: String,
    },
}

/// Builds the assistant message from streamed chunks.
pub(crate) struct Accumulator {
    provider: String,
    model: String,
    timestamp: u64,
    blocks: Vec<Block>,
    /// OpenAI tool-call index -> position in `blocks`.
    tool_slots: HashMap<u64, usize>,
    usage: Usage,
    finish_reason: Option<String>,
}

impl Accumulator {
    pub(crate) fn new(provider: &str, model: &str, timestamp: u64) -> Self {
        Self {
            provider: provider.to_string(),
            model: model.to_string(),
            timestamp,
            blocks: Vec::new(),
            tool_slots: HashMap::new(),
            usage: Usage::default(),
            finish_reason: None,
        }
    }

    pub(crate) fn apply(&mut self, chunk: &Value) -> Result<Vec<Delta>, String> {
        if let Some(error) = chunk.get("error").filter(|e| !e.is_null()) {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| error.to_string());
            return Err(format!("provider error: {message}"));
        }
        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            self.apply_usage(usage);
        }

        let mut deltas = Vec::new();
        let Some(choice) = chunk.get("choices").and_then(|choices| choices.get(0)) else {
            return Ok(deltas);
        };
        if let Some(delta) = choice.get("delta") {
            let reasoning = REASONING_FIELDS.into_iter().find_map(|field| {
                delta
                    .get(field)
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(|text| (field, text))
            });
            if let Some((field, text)) = reasoning {
                match self.blocks.last_mut() {
                    Some(Block::Thinking { text: existing, .. }) => existing.push_str(text),
                    _ => self.blocks.push(Block::Thinking {
                        text: text.to_string(),
                        field,
                    }),
                }
                deltas.push(Delta::Thinking { text: text.to_string() });
            }
            if let Some(text) = delta.get("content").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                match self.blocks.last_mut() {
                    Some(Block::Text(existing)) => existing.push_str(text),
                    _ => self.blocks.push(Block::Text(text.to_string())),
                }
                deltas.push(Delta::Text { text: text.to_string() });
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for (position, call) in calls.iter().enumerate() {
                    self.apply_tool_call(position, call, &mut deltas);
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_string());
        }
        Ok(deltas)
    }

    fn apply_tool_call(&mut self, position: usize, call: &Value, deltas: &mut Vec<Delta>) {
        let index = call.get("index").and_then(Value::as_u64).unwrap_or(position as u64);
        let function = call.get("function");
        let name = function
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let slot = match self.tool_slots.get(&index) {
            Some(&slot) => {
                if let Block::Tool { name: existing, .. } = &mut self.blocks[slot]
                    && existing.is_empty()
                    && !name.is_empty()
                {
                    *existing = name.to_string();
                }
                slot
            }
            None => {
                let id = call
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("call_{index}"));
                self.blocks.push(Block::Tool {
                    id: id.clone(),
                    name: name.to_string(),
                    args: String::new(),
                });
                let slot = self.blocks.len() - 1;
                self.tool_slots.insert(index, slot);
                deltas.push(Delta::ToolCallStart {
                    id,
                    name: name.to_string(),
                });
                slot
            }
        };
        if let Some(fragment) = function
            .and_then(|f| f.get("arguments"))
            .and_then(Value::as_str)
            .filter(|f| !f.is_empty())
            && let Block::Tool { args, id, .. } = &mut self.blocks[slot]
        {
            args.push_str(fragment);
            deltas.push(Delta::ToolCallArgs {
                id: id.clone(),
                text: fragment.to_string(),
            });
        }
    }

    fn apply_usage(&mut self, usage: &Value) {
        let get = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
        self.usage.input = get("prompt_tokens");
        self.usage.output = get("completion_tokens");
        // OpenAI reports cache hits in `prompt_tokens_details.cached_tokens`, DeepSeek in
        // `prompt_cache_hit_tokens` (pi: parseChunkUsage).
        self.usage.cache_read = usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_u64)
            .or_else(|| usage.get("prompt_cache_hit_tokens").and_then(Value::as_u64))
            .unwrap_or(0);
        let total = get("total_tokens");
        self.usage.total_tokens = if total > 0 {
            total
        } else {
            // Counts come from the server, so a bogus value must not overflow.
            self.usage.input.saturating_add(self.usage.output)
        };
    }

    fn content(&self) -> Vec<ContentBlock> {
        self.blocks
            .iter()
            .map(|block| match block {
                Block::Text(text) => ContentBlock::Text { text: text.clone() },
                Block::Thinking { text, field } => ContentBlock::Thinking {
                    thinking: text.clone(),
                    signature: Some((*field).to_string()),
                },
                Block::Tool { id, name, args } => {
                    let (arguments, arguments_error) = parse_arguments(args);
                    ContentBlock::ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments,
                        arguments_error,
                    }
                }
            })
            .collect()
    }

    pub(crate) fn finish(&self) -> Result<AssistantMessage, String> {
        let content = self.content();
        let has_tool_calls = content.iter().any(|b| matches!(b, ContentBlock::ToolCall { .. }));
        let Some(reason) = self.finish_reason.as_deref() else {
            return Err("stream ended without a finish_reason".to_string());
        };
        // pi: openai-completions.ts mapStopReason. Only the known endings succeed; any other
        // value means the server interrupted or rejected the answer, so it is not reported
        // as a complete one.
        let (stop_reason, error_message) = match reason {
            // Some servers report "stop" with tool calls.
            "stop" | "end" if has_tool_calls => (StopReason::ToolUse, None),
            "stop" | "end" => (StopReason::Stop, None),
            "tool_calls" | "function_call" => (StopReason::ToolUse, None),
            "length" => (StopReason::Length, None),
            "content_filter" => (
                StopReason::Error,
                Some("response blocked by the provider content filter".to_string()),
            ),
            other => (
                StopReason::Error,
                Some(format!("the provider ended the response with finish_reason {other:?}")),
            ),
        };
        Ok(AssistantMessage {
            content,
            provider: self.provider.clone(),
            model: self.model.clone(),
            usage: self.usage.clone(),
            stop_reason,
            provider_error: error_message.is_some().then(|| ProviderError::Provider {
                code: Some(reason.to_string()),
            }),
            error_message,
            timestamp: self.timestamp,
            duration_ms: None,
        })
    }

    pub(crate) fn failure(&self, stop_reason: StopReason, error: impl Into<String>) -> AssistantMessage {
        AssistantMessage {
            content: self.content(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            usage: self.usage.clone(),
            stop_reason,
            error_message: Some(error.into()),
            provider_error: None,
            timestamp: self.timestamp,
            duration_ms: None,
        }
    }
}

fn parse_arguments(raw: &str) -> (Value, Option<String>) {
    if raw.trim().is_empty() {
        return (json!({}), None);
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(value) => (value, None),
        Err(e) => (json!({}), Some(format!("arguments are not valid JSON: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::system_prompt::{SectionChange, SystemMessage};
    use crate::ai::types::{ToolDef, ToolResultMessage};

    fn apply_all(acc: &mut Accumulator, chunks: &[Value]) -> Vec<Delta> {
        chunks.iter().flat_map(|c| acc.apply(c).unwrap()).collect()
    }

    #[test]
    fn accumulates_text_tool_calls_and_usage() {
        let mut acc = Accumulator::new("p", "m", 1);
        let deltas = apply_all(
            &mut acc,
            &[
                json!({"choices":[{"delta":{"role":"assistant","content":"Che"}}]}),
                json!({"choices":[{"delta":{"content":"cking."}}]}),
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"bash","arguments":"{\"comm"}}]}}]}),
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"and\":\"ls\"}"}}]}}]}),
                json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
                json!({"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":5,"total_tokens":17,"prompt_tokens_details":{"cached_tokens":4}}}),
            ],
        );
        assert_eq!(deltas.len(), 5);
        let message = acc.finish().unwrap();
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        assert_eq!(message.text(), "Checking.");
        let calls = message.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[0].arguments, json!({"command":"ls"}));
        assert_eq!(
            message.usage,
            Usage {
                input: 12,
                output: 5,
                cache_read: 4,
                total_tokens: 17
            }
        );
    }

    #[test]
    fn reports_invalid_tool_arguments_and_missing_finish_reason() {
        let mut acc = Accumulator::new("p", "m", 1);
        acc.apply(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"bash","arguments":"{\"command\":"}}]}}]}))
            .unwrap();
        assert!(acc.finish().is_err());
        let failed = acc.failure(StopReason::Error, "cut off");
        assert!(failed.tool_calls()[0].arguments_error.is_some());
    }

    #[test]
    fn interleaved_argument_deltas_identify_their_tool_call() {
        let mut acc = Accumulator::new("p", "m", 1);
        let deltas = apply_all(
            &mut acc,
            &[
                json!({"choices":[{"delta":{"tool_calls":[
                    {"index":0,"id":"a","function":{"name":"bash","arguments":"{"}},
                    {"index":1,"id":"b","function":{"name":"read_file","arguments":"{"}}
                ]}}]}),
                json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"function":{"arguments":"}"}},
                {"index":1,"function":{"arguments":"}"}}
            ]},"finish_reason":"tool_calls"}]}),
            ],
        );
        let args: Vec<_> = deltas
            .into_iter()
            .filter_map(|delta| {
                if matches!(delta, Delta::ToolCallArgs { .. }) {
                    Some(serde_json::to_value(delta).unwrap())
                } else {
                    None
                }
            })
            .collect();

        assert_eq!(
            args.iter().map(|a| a["id"].as_str()).collect::<Vec<_>>(),
            [Some("a"), Some("b"), Some("a"), Some("b")]
        );
    }

    #[test]
    fn stop_with_tool_calls_is_tool_use_and_errors_in_chunks_fail() {
        let mut acc = Accumulator::new("p", "m", 1);
        acc.apply(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"x","arguments":"{}"}}]},"finish_reason":"stop"}]}))
            .unwrap();
        assert_eq!(acc.finish().unwrap().stop_reason, StopReason::ToolUse);
        assert!(acc.apply(&json!({"error":{"message":"overloaded"}})).is_err());
    }

    #[test]
    fn only_known_finish_reasons_succeed() {
        let finish = |reason: &str| {
            let mut acc = Accumulator::new("p", "m", 1);
            acc.apply(&json!({"choices":[{"delta":{"content":"x"},"finish_reason":reason}]}))
                .unwrap();
            acc.finish().unwrap()
        };
        assert_eq!(finish("stop").stop_reason, StopReason::Stop);
        assert_eq!(finish("end").stop_reason, StopReason::Stop);
        assert_eq!(finish("length").stop_reason, StopReason::Length);
        for reason in ["content_filter", "network_error", "server_interrupted"] {
            let message = finish(reason);
            assert_eq!(message.stop_reason, StopReason::Error, "{reason}");
            assert_eq!(
                message.provider_error,
                Some(ProviderError::Provider {
                    code: Some(reason.to_string())
                })
            );
            assert!(message.error_message.is_some(), "{reason}");
        }
    }

    #[test]
    fn body_pairs_tool_calls_with_results() {
        let assistant = AssistantMessage {
            content: vec![
                ContentBlock::ToolCall {
                    id: "a".into(),
                    name: "bash".into(),
                    arguments: json!({}),
                    arguments_error: None,
                },
                ContentBlock::ToolCall {
                    id: "b".into(),
                    name: "bash".into(),
                    arguments: json!({}),
                    arguments_error: None,
                },
            ],
            ..AssistantMessage::failure("p", "m", StopReason::ToolUse, "")
        };
        let result = |id: &str| {
            Message::ToolResult(ToolResultMessage {
                tool_call_id: id.into(),
                tool_name: "bash".into(),
                content: "ok".into(),
                is_error: false,
                timestamp: 0,
                duration_ms: None,
            })
        };
        let request = Request {
            model: "m".into(),
            // `b` lost its result (cancelled run); `z` has no call.
            messages: vec![
                system(&[("preamble", Some("sys"))]),
                Message::user("go"),
                Message::Assistant(assistant),
                result("a"),
                result("z"),
                Message::user("next"),
            ],
            tools: vec![],
            max_tokens: Some(100),
        };
        let body = build_body(&request, &p_config(SystemUpdates::default()));
        let roles: Vec<&str> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["system", "user", "assistant", "tool", "tool", "user"]);
        assert_eq!(body["messages"][4]["tool_call_id"], "b");
        assert_eq!(body["messages"][4]["content"], "No result provided");
        assert_eq!(body["max_tokens"], 100);
        assert!(body.get("tools").is_none());
    }

    fn p_config(system_updates: SystemUpdates) -> OpenAiConfig {
        OpenAiConfig {
            provider_name: "p".into(),
            system_updates,
            ..OpenAiConfig::default()
        }
    }

    fn system(changes: &[(&str, Option<&str>)]) -> Message {
        Message::System(SystemMessage {
            sections: changes
                .iter()
                .map(|(name, content)| SectionChange {
                    name: name.to_string(),
                    content: content.map(str::to_string),
                })
                .collect(),
            timestamp: 0,
        })
    }

    /// `(role, content)` of every message the request sends.
    fn sent(messages: Vec<Message>, system_updates: SystemUpdates) -> Vec<(String, String)> {
        let request = Request {
            model: "m".into(),
            messages,
            tools: vec![],
            max_tokens: None,
        };
        let body = build_body(&request, &p_config(system_updates));
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| {
                let content = m["content"].as_str().unwrap_or_default();
                (m["role"].as_str().unwrap().to_string(), content.to_string())
            })
            .collect()
    }

    /// A session resumed with a new start time.
    fn resumed() -> Vec<Message> {
        vec![
            system(&[("preamble", Some("Do it.")), ("environment", Some("t1"))]),
            Message::user("a"),
            system(&[("environment", Some("t2"))]),
            Message::user("b"),
        ]
    }

    fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
        expected
            .iter()
            .map(|(role, content)| (role.to_string(), content.to_string()))
            .collect()
    }

    #[test]
    fn collapse_sends_the_latest_prompt_first_and_nothing_else() {
        assert_eq!(
            sent(resumed(), SystemUpdates::Collapse),
            pairs(&[
                ("system", "Do it.\n\n<environment>\nt2\n</environment>"),
                ("user", "a"),
                ("user", "b"),
            ])
        );
    }

    #[test]
    fn inline_sends_each_change_where_it_happened() {
        assert_eq!(
            sent(resumed(), SystemUpdates::Inline),
            pairs(&[
                ("system", "Do it.\n\n<environment>\nt1\n</environment>"),
                ("user", "a"),
                (
                    "system",
                    "Updated system prompt section \"environment\":\n\n<environment>\nt2\n</environment>"
                ),
                ("user", "b"),
            ])
        );
    }

    #[derive(Debug)]
    struct Layer(&'static str, Option<Box<Layer>>);

    impl std::fmt::Display for Layer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }

    impl std::error::Error for Layer {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.1
                .as_deref()
                .map(|cause| cause as &(dyn std::error::Error + 'static))
        }
    }

    #[test]
    fn error_text_includes_causes_once() {
        let refused = Layer("Connection refused", None);
        let connect = Layer("tcp connect error: Connection refused", Some(Box::new(refused)));
        let error = Layer("error sending request", Some(Box::new(connect)));
        assert_eq!(
            with_causes(&error),
            "error sending request: tcp connect error: Connection refused"
        );
    }

    #[test]
    fn content_filter_ends_with_an_error() {
        let mut acc = Accumulator::new("p", "m", 1);
        apply_all(
            &mut acc,
            &[json!({"choices":[{"delta":{"content":"partial"},"finish_reason":"content_filter"}]})],
        );
        let message = acc.finish().unwrap();
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(message.text(), "partial");
        assert!(message.error_message.unwrap().contains("content filter"));
    }

    #[test]
    fn tool_calls_without_an_id_are_named_by_index() {
        let mut acc = Accumulator::new("p", "m", 1);
        apply_all(
            &mut acc,
            &[
                json!({"choices":[{"delta":{"tool_calls":[{"index":2,"function":{"name":"bash","arguments":"{}"}}]}}]}),
                json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
            ],
        );
        assert_eq!(acc.finish().unwrap().tool_calls()[0].id, "call_2");
    }

    #[test]
    fn records_reasoning_from_the_reasoning_field() {
        let mut acc = Accumulator::new("p", "m", 1);
        apply_all(
            &mut acc,
            &[json!({"choices":[{"delta":{"reasoning":"think"},"finish_reason":"stop"}]})],
        );
        let expected = ContentBlock::Thinking {
            thinking: "think".into(),
            signature: Some("reasoning".into()),
        };
        assert_eq!(acc.finish().unwrap().content, [expected]);
    }

    #[test]
    fn reads_cache_hits_from_prompt_cache_hit_tokens() {
        let mut acc = Accumulator::new("p", "m", 1);
        acc.apply(
            &json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2,"prompt_cache_hit_tokens":6}}),
        )
        .unwrap();
        assert_eq!(acc.usage.cache_read, 6);
    }

    #[test]
    fn a_missing_total_is_computed_without_overflowing() {
        let mut acc = Accumulator::new("p", "m", 1);
        acc.apply(&json!({"choices":[],"usage":{"prompt_tokens":u64::MAX,"completion_tokens":1}}))
            .unwrap();
        assert_eq!(acc.usage.total_tokens, u64::MAX);
    }

    #[test]
    fn debug_output_never_contains_the_api_key() {
        let config = OpenAiConfig {
            api_key: Some("sk-debug-secret".into()),
            ..OpenAiConfig::default()
        };
        let debug = format!("{config:?}");
        assert!(!debug.contains("sk-debug-secret"), "{debug}");
        assert!(debug.contains("[REDACTED]"), "{debug}");
    }

    #[test]
    fn records_the_field_reasoning_arrived_in() {
        let mut acc = Accumulator::new("p", "m", 1);
        apply_all(
            &mut acc,
            &[
                json!({"choices":[{"delta":{"reasoning_content":"Think"}}]}),
                json!({"choices":[{"delta":{"reasoning_content":"ing."}}]}),
                json!({"choices":[{"delta":{"content":"Done."},"finish_reason":"stop"}]}),
            ],
        );
        assert_eq!(acc.finish().unwrap().content[0], thinking("Thinking."));
    }

    fn thinking(text: &str) -> ContentBlock {
        ContentBlock::Thinking {
            thinking: text.into(),
            signature: Some("reasoning_content".into()),
        }
    }

    fn reply(model: &str, content: Vec<ContentBlock>) -> Message {
        Message::Assistant(AssistantMessage {
            content,
            ..AssistantMessage::failure("p", model, StopReason::Stop, "")
        })
    }

    fn text(text: &str) -> ContentBlock {
        ContentBlock::Text { text: text.into() }
    }

    /// The assistant entries of a request to provider `p`, model `m`.
    fn assistant_entries(messages: Vec<Message>) -> Vec<Value> {
        let request = Request {
            model: "m".into(),
            messages,
            tools: vec![],
            max_tokens: None,
        };
        let body = build_body(&request, &p_config(SystemUpdates::default()));
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "assistant")
            .cloned()
            .collect()
    }

    #[test]
    fn replays_reasoning_under_the_field_it_arrived_in() {
        let entries = assistant_entries(vec![
            Message::user("a"),
            reply("m", vec![thinking("plan"), text("one")]),
        ]);
        assert_eq!(entries[0]["reasoning_content"], "plan");
    }

    #[test]
    fn drops_reasoning_from_other_models() {
        let entries = assistant_entries(vec![
            Message::user("a"),
            reply("other", vec![thinking("plan"), text("one")]),
        ]);
        assert!(entries[0].get("reasoning_content").is_none());
    }

    #[test]
    fn fills_empty_reasoning_once_the_history_carries_some() {
        // pi #3668: DeepSeek rejects a history in which a response without reasoning lacks the field.
        let entries = assistant_entries(vec![
            Message::user("a"),
            reply("m", vec![thinking("plan"), text("one")]),
            Message::user("b"),
            reply("m", vec![text("two")]),
        ]);
        assert_eq!(entries[1]["reasoning_content"], "");
    }

    fn replayable_request() -> Request {
        Request {
            model: "m".into(),
            messages: vec![
                system(&[("preamble", Some("Be brief."))]),
                Message::user("first"),
                system(&[("environment", Some("changed"))]),
                Message::user("second"),
            ],
            tools: vec![ToolDef {
                name: "bash".into(),
                description: "Run a command".into(),
                parameters: json!({"type": "object"}),
            }],
            max_tokens: Some(64),
        }
    }

    #[test]
    fn replayed_bodies_match_the_live_body_in_both_system_update_modes() {
        let request = replayable_request();
        for system_updates in [SystemUpdates::Collapse, SystemUpdates::Inline] {
            let live = OpenAiConfig {
                base_url: "https://example.invalid/v1".into(),
                api_key: Some("sk-not-recorded".into()),
                ..p_config(system_updates)
            };
            let provider = OpenAiCompatible::new(live.clone()).unwrap();
            let replayed = replay_body(&request, provider.name(), &provider.request_options()).unwrap();
            assert_eq!(replayed, build_body(&request, &live), "{system_updates:?}");
        }
    }

    #[test]
    fn request_options_hold_only_the_conversion_policy() {
        let provider = OpenAiCompatible::new(OpenAiConfig {
            base_url: "https://example.invalid/v1".into(),
            api_key: Some("sk-not-recorded".into()),
            ..p_config(SystemUpdates::Inline)
        })
        .unwrap();
        assert_eq!(
            provider.request_options(),
            json!({"format": "openai-chat-completions", "version": 1, "systemUpdates": "inline"})
        );
    }

    #[test]
    fn replay_rejects_unknown_conversion_options() {
        let request = replayable_request();
        for options in [
            Value::Null,
            json!({"format": "other", "version": 1, "systemUpdates": "collapse"}),
            json!({"format": "openai-chat-completions", "version": 2, "systemUpdates": "collapse"}),
            json!({"format": "openai-chat-completions", "version": 1, "systemUpdates": "sideways"}),
            json!({"format": "openai-chat-completions", "version": 1, "systemUpdates": "collapse", "extra": 1}),
        ] {
            assert!(replay_body(&request, "p", &options).is_err(), "{options}");
        }
    }
}
