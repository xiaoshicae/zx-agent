//! Provider-neutral message and streaming types (pi: packages/ai/src/types.ts).

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::system_prompt::SystemMessage;

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    // The only failure is a system clock set before 1970; a zero timestamp is harmless then.
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, duration_ms)
}

/// Whole milliseconds of `duration`.
pub fn duration_ms(duration: Duration) -> u64 {
    // u64 milliseconds cover about 584 million years; anything past them saturates.
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Token counts of one response, or a sum of several. The counts a server did not report
/// are 0 and left out of the JSON, so records written before they existed read back
/// unchanged.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    /// Prompt tokens, including those served from or written to the provider cache.
    pub input: u64,
    /// Completion tokens, reasoning included.
    pub output: u64,
    /// Prompt tokens served from the provider cache.
    pub cache_read: u64,
    /// Prompt tokens written to the provider cache (pi: `cacheWrite`).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cache_write: u64,
    /// Reasoning tokens, counted within `output` (pi: `reasoning`).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reasoning: u64,
    pub total_tokens: u64,
    /// What the response cost by the configured prices ([`ModelCost`]), in their unit;
    /// absent without prices. A sum adds the known costs (pi: `cost.total`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cost: Option<f64>,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl Usage {
    /// Input plus output tokens, saturating: what a token budget counts.
    pub fn input_and_output(&self) -> u64 {
        self.input.saturating_add(self.output)
    }

    /// Price these counts by the cost of `model` in `models`, the model that was requested:
    /// that is the name the audit records, while a server may report another, such as a dated
    /// version of an alias. Without a known cost, the usage stays unpriced.
    pub fn price(&mut self, models: &HashMap<String, ModelMeta>, model: &str) {
        if let Some(cost) = models.get(model).and_then(|meta| meta.cost) {
            self.total_cost = Some(cost.of(self));
        }
    }

    /// Add `other` to these counts, saturating: servers report them, so they are untrusted.
    pub fn add(&mut self, other: &Usage) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
        self.reasoning = self.reasoning.saturating_add(other.reasoning);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
        self.total_cost = match (self.total_cost, other.total_cost) {
            (None, None) => None,
            (mine, theirs) => Some(mine.unwrap_or(0.0) + theirs.unwrap_or(0.0)),
        };
    }
}

/// What the host knows about a model, from its configuration (pi: `Model`). There is no
/// built-in table of models: a service's models and prices are the host's to configure.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelMeta {
    /// Tokens the model's context holds, input and output together; `None` when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Prices, for the cost of each response; `None` leaves costs unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<ModelCost>,
}

/// Prices per million tokens (pi: `ModelCost`), in one currency of the host's choosing:
/// costs come out in that unit, and a cost budget is read in it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    /// Prompt tokens neither served from nor written to the cache.
    pub input: f64,
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write: f64,
}

impl ModelCost {
    /// The cost of `usage` in the prices' unit: the prompt tokens outside the cache at the input price,
    /// the cached ones at theirs, the output at its price.
    pub fn of(&self, usage: &Usage) -> f64 {
        let fresh = usage
            .input
            .saturating_sub(usage.cache_read)
            .saturating_sub(usage.cache_write);
        // Token counts are far below 2^53, so the conversions are exact.
        let tokens = |count: u64, price: f64| count as f64 * price;
        (tokens(fresh, self.input)
            + tokens(usage.cache_read, self.cache_read)
            + tokens(usage.cache_write, self.cache_write)
            + tokens(usage.output, self.output))
            / 1_000_000.0
    }
}

/// Why a response ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    /// The model finished its answer.
    Stop,
    /// Output hit the token limit; tool-call arguments may be truncated.
    Length,
    /// The model asked for tool calls.
    ToolUse,
    /// The request failed. `error_message` says why.
    Error,
    /// The request was cancelled. Content generated so far is kept.
    Aborted,
}

/// One block of assistant output, in the order the model produced it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        /// What the provider needs to send the block back. For OpenAI-compatible servers, the
        /// response field the text arrived in, such as `reasoning_content`
        /// (pi: `ThinkingContent.thinkingSignature`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
        /// Set when the streamed arguments were not valid JSON (pi: `ToolCall.argumentsError`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        arguments_error: Option<String>,
    },
}

/// A tool call extracted from an assistant message. In JSON its fields are named as in the
/// message's `toolCall` block.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments_error: Option<String>,
}

/// Machine-readable provider failure details. Human-readable text stays in
/// AssistantMessage::error_message. Optional metadata keeps older sessions readable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum ProviderError {
    /// An unsuccessful HTTP response.
    Http {
        status: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        /// A numeric Retry-After header, converted to milliseconds.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after_ms: Option<u64>,
    },
    /// An error reported inside a successful streaming HTTP response.
    Provider {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
    },
    /// A connection or response-body transport failure.
    Transport,
    /// A connection, response-header, or stream idle timeout.
    Timeout,
    /// Invalid JSON, missing completion markers, or another protocol violation.
    Protocol,
    /// A configured response or event byte budget was exceeded.
    ResourceLimit,
    /// The host's API key source failed, so no request was sent.
    ApiKey,
}

/// A model response, complete or failed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
    pub provider: String,
    pub model: String,
    pub usage: Usage,
    pub stop_reason: StopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_error: Option<ProviderError>,
    /// When the request started.
    pub timestamp: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// The ID the server gave the response (pi: `responseId`), when it sent one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    /// The model the server reports having used, when it names one other than `model` (pi:
    /// `responseModel`), such as a dated version of an alias.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_model: Option<String>,
}

impl AssistantMessage {
    /// A message for a request that failed before or during generation.
    pub fn failure(provider: &str, model: &str, stop_reason: StopReason, error: impl Into<String>) -> Self {
        Self {
            content: Vec::new(),
            provider: provider.to_string(),
            model: model.to_string(),
            usage: Usage::default(),
            stop_reason,
            error_message: Some(error.into()),
            provider_error: None,
            timestamp: now_ms(),
            duration_ms: None,
            response_id: None,
            response_model: None,
        }
    }

    /// Why a failed response failed: its `error_message`, or one default text for a failed
    /// response that carries none, so that every report of a failure names a reason.
    pub fn failure_reason(&self) -> &str {
        self.error_message
            .as_deref()
            .unwrap_or("the request failed without an error message")
    }

    /// Concatenated text blocks.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Tool calls in the order the model issued them.
    pub fn tool_calls(&self) -> Vec<ToolCall> {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                    arguments_error,
                } => Some(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                    arguments_error: arguments_error.clone(),
                }),
                _ => None,
            })
            .collect()
    }
}

/// The result of one tool call, sent back to the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: String,
    pub is_error: bool,
    pub timestamp: u64,
    /// How long `execute()` took; absent when the tool did not run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Structured data for frontends and logs (pi: `details`). Recorded with the result but
    /// never sent to the model: providers send `content` only, and summary requests and
    /// context sizes leave it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// What the tool spent on work of its own, such as a model it asked (pi: a result's
    /// `usage`). Recorded with the result and never sent to the model, like `details`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

/// Whether user text starts a new answer, continues the preceding answer, or is a context
/// summary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UserMessageKind {
    #[default]
    Prompt,
    ContinueAnswer,
    /// Built by the session from a compaction record in place of the summarized messages.
    /// It is not a stored message, so context edits cannot target it; editing one of its
    /// sources invalidates the summary instead.
    Summary,
    /// Built by the session from a `custom_message` record that an extension appended (pi:
    /// a `custom` message, which the model sees as a user message). Context edits target the
    /// record.
    Custom,
}

impl UserMessageKind {
    fn is_prompt(&self) -> bool {
        *self == Self::Prompt
    }
}

/// A transcript message. Ordinary user records retain their pre-Hooks JSON shape.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    System(SystemMessage),
    User {
        content: String,
        timestamp: u64,
        #[serde(default, skip_serializing_if = "UserMessageKind::is_prompt")]
        kind: UserMessageKind,
    },
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
}

impl Message {
    /// A user message stamped with the current time.
    pub fn user(text: impl Into<String>) -> Self {
        Message::User {
            content: text.into(),
            timestamp: now_ms(),
            kind: UserMessageKind::Prompt,
        }
    }

    /// A request to continue the same answer, independent of the prompt's wording.
    pub fn continuation(text: impl Into<String>) -> Self {
        Self::User {
            content: text.into(),
            timestamp: now_ms(),
            kind: UserMessageKind::ContinueAnswer,
        }
    }

    /// A projection needs a separate copy; the original audit record stays unchanged.
    /// Public only for the session in zx-core, not part of the API.
    #[doc(hidden)]
    pub fn with_replaced_text(&self, text: &str) -> Self {
        let mut message = self.clone();
        match &mut message {
            // System changes are appended, never edited.
            Self::System(_) => {}
            Self::User { content, .. } => *content = text.to_string(),
            Self::Assistant(assistant) => {
                assistant.content = vec![ContentBlock::Text { text: text.to_string() }];
            }
            Self::ToolResult(result) => result.content = text.to_string(),
        }
        message
    }

    /// The serialized role name: `system`, `user`, `assistant`, or `toolResult`.
    pub fn role(&self) -> &'static str {
        match self {
            Message::System(_) => "system",
            Message::User { .. } => "user",
            Message::Assistant(_) => "assistant",
            Message::ToolResult(_) => "toolResult",
        }
    }
}

/// Tool declaration sent to the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema of the arguments object.
    pub parameters: Value,
}

/// One model request. The system prompt travels in `messages` as system messages.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub max_tokens: Option<u32>,
    /// How hard a reasoning model should think. `None` sends nothing, leaving the server's
    /// default; each provider maps a level to its own settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,
    /// A key for the server's prompt cache, such as the session ID, so that requests of one
    /// conversation reuse the same cache (pi: `prompt_cache_key` from the session ID).
    /// Providers send it only when configured to; the request audit records it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_key: Option<String>,
}

/// How hard a reasoning model thinks before it answers (pi: `ThinkingLevel`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reasoning {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
}

impl Reasoning {
    /// Every level, from least to most reasoning.
    pub const ALL: [Reasoning; 6] = [
        Reasoning::Off,
        Reasoning::Minimal,
        Reasoning::Low,
        Reasoning::Medium,
        Reasoning::High,
        Reasoning::Xhigh,
    ];

    /// The level's name, as configuration files and commands write it.
    pub fn as_str(self) -> &'static str {
        match self {
            Reasoning::Off => "off",
            Reasoning::Minimal => "minimal",
            Reasoning::Low => "low",
            Reasoning::Medium => "medium",
            Reasoning::High => "high",
            Reasoning::Xhigh => "xhigh",
        }
    }
}

impl std::fmt::Display for Reasoning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Reasoning {
    type Err = String;

    /// A level by its name, ignoring case.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let wanted = text.trim().to_ascii_lowercase();
        Reasoning::ALL
            .into_iter()
            .find(|level| level.as_str() == wanted)
            .ok_or_else(|| {
                let names: Vec<&str> = Reasoning::ALL.iter().map(|level| level.as_str()).collect();
                format!("unknown reasoning level {text:?}; use one of {}", names.join(", "))
            })
    }
}

/// Incremental output while a response streams.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Delta {
    Text {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolCallStart {
        id: String,
        name: String,
    },
    ToolCallArgs {
        /// Matches ToolCallStart::id. Empty only when decoding older uncorrelated events.
        #[serde(default)]
        id: String,
        text: String,
    },
}

/// Event protocol of one streamed response (pi: `AssistantMessageEvent`).
///
/// A stream ends with exactly one `Done` or `Error`. `Start` precedes deltas but may be
/// missing when the request fails before generation begins.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamEvent {
    Start,
    Delta(Delta),
    Done(AssistantMessage),
    Error(AssistantMessage),
}

impl StreamEvent {
    /// The event that ends a stream with `message`: `Error` when it failed or was aborted,
    /// `Done` otherwise.
    pub fn ending(message: AssistantMessage) -> Self {
        match message.stop_reason {
            StopReason::Error | StopReason::Aborted => Self::Error(message),
            StopReason::Stop | StopReason::Length | StopReason::ToolUse => Self::Done(message),
        }
    }
}

#[cfg(test)]
mod tests;
