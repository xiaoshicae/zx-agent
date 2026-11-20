//! Provider-neutral message and streaming types (pi: packages/ai/src/types.ts).

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use zx_util::text::prefix_noted;

use super::system_prompt::SystemMessage;

/// The most of an invalid argument text that its error keeps: enough for the model to see
/// what it sent, bounded so the session and the next request do not grow with it.
const MAX_RECEIVED_ARGUMENTS: usize = 2048;

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
/// are 0 and left out of the JSON.
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
    #[serde(default, skip_serializing_if = "zx_util::default::is_default")]
    pub cache_write: u64,
    /// Reasoning tokens, counted within `output` (pi: `reasoning`).
    #[serde(default, skip_serializing_if = "zx_util::default::is_default")]
    pub reasoning: u64,
    /// All tokens of the response: the total the server reports, or input plus output where
    /// it reports none.
    pub total_tokens: u64,
    /// What the response cost by the configured prices ([`ModelCost`]), in their unit;
    /// absent without prices. A sum adds the known costs (pi: `cost.total`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cost: Option<f64>,
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
    /// Whether the model takes images (pi: `input` includes `image`). A request to a model
    /// that does not carries a note in place of the images ([`Request::takes_images`]).
    #[serde(default, skip_serializing_if = "zx_util::default::is_default")]
    pub takes_images: bool,
}

/// Prices per million tokens (pi: `ModelCost`), in one currency of the host's choosing:
/// costs come out in that unit, and a cost budget is read in it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ModelCost {
    /// Prompt tokens neither served from nor written to the cache.
    pub input: f64,
    /// Output tokens, reasoning included.
    pub output: f64,
    /// Prompt tokens served from the cache.
    #[serde(default)]
    pub cache_read: f64,
    /// Prompt tokens written to the cache.
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

impl StopReason {
    /// The reason as JSON names it, such as `toolUse`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Length => "length",
            Self::ToolUse => "toolUse",
            Self::Error => "error",
            Self::Aborted => "aborted",
        }
    }
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
        /// response field the text arrived in, such as `reasoning_content`; for the Anthropic
        /// Messages API, the block's signature, or `redacted:` and the encrypted data of a
        /// block the API redacted (pi: `ThinkingContent.thinkingSignature`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    ToolCall(ToolCall),
}

/// A tool call the model asked for: the `toolCall` block of an assistant message, whose JSON
/// holds these fields beside the block's `type`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    /// Set when the streamed arguments were not valid JSON (pi: `ToolCall.argumentsError`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments_error: Option<String>,
}

impl ToolCall {
    /// The call `id` to the tool `name` whose arguments streamed as the JSON text `raw`, for
    /// providers: no text is an empty object, and text that does not parse is an empty object
    /// with an `arguments_error` the model reads, quoting what it sent.
    pub fn streamed(id: String, name: String, raw: &str) -> Self {
        let (arguments, arguments_error) = if raw.trim().is_empty() {
            (json!({}), None)
        } else {
            match serde_json::from_str::<Value>(raw) {
                Ok(value) => (value, None),
                Err(error) => {
                    let received = prefix_noted(raw, MAX_RECEIVED_ARGUMENTS, |_, _| "...".to_string());
                    let error = format!("arguments are not valid JSON: {error}\n\nReceived arguments:\n{received}");
                    (json!({}), Some(error))
                }
            }
        };
        Self {
            id,
            name,
            arguments,
            arguments_error,
        }
    }
}

/// Why a model request failed, as its provider reports it: the kind of failure, from which
/// alone the kernel decides what to do next, and what the server said, kept for the record.
/// The text for people is the message's `error_message`. Absent fields are left out of the
/// JSON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderError {
    /// What kind of failure it was: what the kernel decides from.
    pub kind: ProviderErrorKind,
    /// The HTTP status of the response, when the server answered with an error status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// The server's error code as it sent it, or the reason it gave for ending a response
    /// with something other than an answer, such as `content_filter`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// How long the server asked the client to wait before trying again, in milliseconds,
    /// whatever form the provider read it in, such as a `Retry-After` header in seconds or as
    /// a date. A retry waits at least this long, up to the retry policy's longest delay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
}

impl From<ProviderErrorKind> for ProviderError {
    /// A failure of `kind` that the server said nothing more about.
    fn from(kind: ProviderErrorKind) -> Self {
        Self {
            kind,
            status: None,
            code: None,
            retry_after_ms: None,
        }
    }
}

/// What kind of failure ended a model request. The kinds name situations, not services: a
/// provider maps what its service reports (statuses, error codes, wording) to them, and the
/// kernel decides from the kind alone whether to try the request again
/// ([`is_retryable`](crate::retry::is_retryable)) or to compact the context
/// ([`is_context_overflow`](crate::retry::is_context_overflow)).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorKind {
    /// The service limits how often it is asked: an HTTP 429 or a rate limit error.
    RateLimited,
    /// The service is overloaded, failing or otherwise unavailable for now: an HTTP 5xx or an
    /// error that says so.
    Unavailable,
    /// The connection failed, or broke off before the response ended, as does a stream that
    /// ends without saying how the answer ended.
    Transport,
    /// The service did not answer, or stopped sending, within the time allowed, or says that
    /// it timed out itself: an HTTP 408 or an error that says so.
    Timeout,
    /// The request does not fit the model's context window: sending it again fails again,
    /// a smaller context may succeed.
    ContextOverflow,
    /// The service refused the client's credentials, permissions or quota, which someone has
    /// to fix before a request can succeed.
    AccessDenied,
    /// The host's API key source failed, so no request was sent.
    ApiKey,
    /// The service ended the response with a reason of its own instead of an answer, such as
    /// a content filter; the error's `code` holds the reason.
    Refused,
    /// The response broke the protocol: data that cannot be read, or a whole answer where a
    /// stream was asked for.
    Protocol,
    /// The response exceeded a byte limit of the provider's settings.
    ResourceLimit,
    /// Any other failure, such as a request the service rejected as invalid.
    Other,
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
    /// What kind of failure ended the request, as its provider reports it. Absent for a
    /// response that did not fail, for a cancelled one, and for a failure its provider did
    /// not classify, which the kernel does not try again.
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
    /// What the API changed in the request before the model read it, as the API reports it
    /// (pi: the `anthropic_input_transformations` diagnostic), such as earlier thinking it
    /// dropped. Empty when it reported nothing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_transformations: Vec<InputTransformation>,
}

/// One change an API made to a request before the model read it, in the API's words, such as
/// `{"type": "thinking_dropped", "path": "messages.1.content.0", "reason":
/// "prefix_binding_mismatch"}` from the Anthropic Messages API. Values this crate does not
/// know are kept as sent: an API adds new ones.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputTransformation {
    /// What the API did, such as `thinking_dropped`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Where in the request as the API received it, such as `messages.1.content.0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Why, such as `prefix_binding_mismatch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl AssistantMessage {
    /// An answer of `model`, through `provider`, that stopped for `stop_reason` with
    /// `content`: no error, and no usage or timing yet.
    pub fn new(provider: &str, model: &str, stop_reason: StopReason, content: Vec<ContentBlock>) -> Self {
        Self {
            content,
            provider: provider.to_string(),
            model: model.to_string(),
            usage: Usage::default(),
            stop_reason,
            error_message: None,
            provider_error: None,
            timestamp: now_ms(),
            duration_ms: None,
            response_id: None,
            response_model: None,
            input_transformations: Vec::new(),
        }
    }

    /// A message for a request that failed before or during generation.
    pub fn failure(provider: &str, model: &str, stop_reason: StopReason, error: impl Into<String>) -> Self {
        Self {
            error_message: Some(error.into()),
            ..Self::new(provider, model, stop_reason, Vec::new())
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
                ContentBlock::ToolCall(call) => Some(call.clone()),
                _ => None,
            })
            .collect()
    }
}

/// An image given to the model (pi: `ImageContent`): the bytes of a PNG, JPEG, GIF or WebP
/// file, in standard base64, and their media type. Whoever takes an image in fits it to the
/// limits of the model APIs first (`zx_agent::builtin::images`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Image {
    /// The media type, such as `image/png`.
    pub mime_type: String,
    /// The bytes, in standard base64 with padding.
    pub data: String,
}

/// What a provider sends in place of the images of a user message when the model takes none
/// (pi: `NON_VISION_USER_IMAGE_PLACEHOLDER`).
pub const USER_IMAGES_OMITTED: &str = "(image omitted: model does not support images)";

/// What a provider sends in place of the images of a tool result when the model takes none
/// (pi: `NON_VISION_TOOL_IMAGE_PLACEHOLDER`).
pub const TOOL_IMAGES_OMITTED: &str = "(tool image omitted: model does not support images)";

/// `text` followed by `note` on a line of its own, or `note` alone when `text` is blank: how
/// a provider says that something of a message was left out.
pub fn with_note(text: &str, note: &str) -> String {
    if text.trim().is_empty() {
        note.to_string()
    } else {
        format!("{text}\n{note}")
    }
}

/// What a provider sends the model as the result of a tool call whose result is empty.
pub const EMPTY_TOOL_RESULT: &str = "(no output)";

/// What a provider sends the model as the result of a tool call that has none, such as one a
/// cancelled run left behind: the APIs refuse a call without a result.
pub const MISSING_TOOL_RESULT: &str = "No result provided";

/// The result of one tool call, sent back to the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: String,
    /// Images the tool returned (pi: the image blocks of a result's `content`), which the
    /// model sees after the text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<Image>,
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

/// A transcript message. A user message of the default kind, a prompt, is written without
/// its `kind`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    System(SystemMessage),
    User {
        content: String,
        /// Images sent with the text (pi: the image blocks of a user message's `content`),
        /// which the model sees before the text.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<Image>,
        timestamp: u64,
        #[serde(default, skip_serializing_if = "zx_util::default::is_default")]
        kind: UserMessageKind,
    },
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
}

impl Message {
    /// A user message stamped with the current time.
    pub fn user(text: impl Into<String>) -> Self {
        Self::user_with_images(text, Vec::new())
    }

    /// A user message of `text` and `images`, stamped with the current time.
    pub fn user_with_images(text: impl Into<String>, images: Vec<Image>) -> Self {
        Self::User {
            content: text.into(),
            images,
            timestamp: now_ms(),
            kind: UserMessageKind::Prompt,
        }
    }

    /// A request to continue the same answer, independent of the prompt's wording.
    pub fn continuation(text: impl Into<String>) -> Self {
        Self::User {
            content: text.into(),
            images: Vec::new(),
            timestamp: now_ms(),
            kind: UserMessageKind::ContinueAnswer,
        }
    }

    /// Who the message is from: its `role`, as the session file writes it.
    pub fn role(&self) -> Role {
        match self {
            Self::System(_) => Role::System,
            Self::User { .. } => Role::User,
            Self::Assistant(_) => Role::Assistant,
            Self::ToolResult(_) => Role::ToolResult,
        }
    }

    /// Which kind of user message this is; `None` for a message of another role.
    pub fn user_kind(&self) -> Option<UserMessageKind> {
        match self {
            Self::User { kind, .. } => Some(*kind),
            Self::System(_) | Self::Assistant(_) | Self::ToolResult(_) => None,
        }
    }
}

/// Who a [`Message`] is from, serialized as its `role`: `system`, `user`, `assistant` or
/// `toolResult`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Role {
    System,
    User,
    Assistant,
    ToolResult,
}

/// `text` in the characters model APIs take in a tool's name or a call's ID: ASCII letters,
/// digits, `_` and `-`, with every other character replaced by `_`. How long it may be is the
/// caller's to bound.
pub fn api_identifier(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Tool declaration sent to the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDef {
    /// The name the model calls the tool by.
    pub name: String,
    /// What the tool does, for the model to choose it by.
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
    /// The most tokens the answer may take. `None` leaves it to the provider: the
    /// OpenAI-compatible one sends no limit, and the Anthropic one, whose API requires a limit,
    /// sends a default of its own.
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
    /// Whether the model takes images ([`ModelMeta::takes_images`]). When it does not, a
    /// provider sends [`USER_IMAGES_OMITTED`] or [`TOOL_IMAGES_OMITTED`] in place of the
    /// images of a message (pi: `downgradeUnsupportedImages`); the session keeps them.
    #[serde(default, skip_serializing_if = "zx_util::default::is_default")]
    pub takes_images: bool,
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

/// Incremental output while a response streams. Events carry it as JSON for their readers;
/// nothing here reads one back.
#[derive(Clone, Debug, PartialEq, Serialize)]
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
        /// The `id` of the `ToolCallStart` these arguments belong to.
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
