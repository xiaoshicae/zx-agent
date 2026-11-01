//! Provider-neutral message and streaming types (pi: packages/ai/src/types.ts).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::system_prompt::SystemMessage;

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    // The only failure is a system clock set before 1970; a zero timestamp is harmless then.
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(duration_ms)
        .unwrap_or(0)
}

/// Whole milliseconds of `duration`.
pub fn duration_ms(duration: Duration) -> u64 {
    // u128 -> u64: u64 milliseconds cover about 584 million years, so this cannot truncate in practice.
    duration.as_millis() as u64
}

/// Token counts of one response, or a sum of several.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    /// Prompt tokens, including those served from the provider cache.
    pub input: u64,
    pub output: u64,
    /// Prompt tokens served from the provider cache.
    pub cache_read: u64,
    pub total_tokens: u64,
}

impl Usage {
    /// Add `other` to these counts, saturating: servers report them, so they are untrusted.
    pub fn add(&mut self, other: &Usage) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
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

/// A tool call extracted from an assistant message.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
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
        }
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
}

/// Whether user text starts a new answer or continues the preceding answer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UserMessageKind {
    #[default]
    Prompt,
    ContinueAnswer,
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
    pub(crate) fn with_replaced_text(&self, text: &str) -> Self {
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
#[derive(Clone, Debug)]
pub struct Request {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub max_tokens: Option<u32>,
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::ai::{SectionChange, SystemMessage};

    #[test]
    fn reads_older_uncorrelated_deltas() {
        let delta: Delta = serde_json::from_value(json!({"kind": "tool_call_args", "text": "{}"})).unwrap();
        assert_eq!(
            delta,
            Delta::ToolCallArgs {
                id: String::new(),
                text: "{}".into()
            }
        );
    }

    #[test]
    fn provider_error_metadata_is_optional_and_round_trips() {
        let mut message = AssistantMessage::failure("p", "m", StopReason::Error, "rate limited");
        let legacy = serde_json::to_value(&message).unwrap();
        assert!(legacy.get("providerError").is_none());
        assert_eq!(
            serde_json::from_value::<AssistantMessage>(legacy)
                .unwrap()
                .provider_error,
            None
        );
        message.provider_error = Some(ProviderError::Http {
            status: 429,
            code: Some("rate_limit_exceeded".into()),
            retry_after_ms: Some(2000),
        });
        let encoded = serde_json::to_value(&message).unwrap();
        assert_eq!(encoded["providerError"]["retryAfterMs"], 2000);
        assert_eq!(serde_json::from_value::<AssistantMessage>(encoded).unwrap(), message);
    }

    fn assistant() -> AssistantMessage {
        AssistantMessage {
            content: vec![
                ContentBlock::Thinking {
                    thinking: "plan".into(),
                    signature: Some("reasoning_content".into()),
                },
                ContentBlock::Text {
                    text: "Checking.".into(),
                },
                ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: json!({"command": "ls"}),
                    arguments_error: None,
                },
            ],
            provider: "p".into(),
            model: "m".into(),
            usage: Usage {
                input: 3,
                output: 2,
                cache_read: 1,
                total_tokens: 5,
            },
            stop_reason: StopReason::ToolUse,
            error_message: None,
            provider_error: None,
            timestamp: 7,
            duration_ms: None,
        }
    }

    #[test]
    fn messages_keep_the_session_file_format() {
        // Session files store messages in exactly this shape; changing it breaks existing files.
        let cases = [
            (
                Message::System(SystemMessage {
                    sections: vec![SectionChange {
                        name: "preamble".into(),
                        content: Some("Do it.".into()),
                    }],
                    timestamp: 1,
                }),
                json!({"role": "system", "sections": [{"name": "preamble", "content": "Do it."}], "timestamp": 1}),
            ),
            (
                Message::User {
                    content: "go".into(),
                    timestamp: 2,
                    kind: UserMessageKind::Prompt,
                },
                json!({"role": "user", "content": "go", "timestamp": 2}),
            ),
            (
                Message::Assistant(assistant()),
                json!({
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "plan", "signature": "reasoning_content"},
                        {"type": "text", "text": "Checking."},
                        {"type": "toolCall", "id": "c1", "name": "bash", "arguments": {"command": "ls"}}
                    ],
                    "provider": "p",
                    "model": "m",
                    "usage": {"input": 3, "output": 2, "cacheRead": 1, "totalTokens": 5},
                    "stopReason": "toolUse",
                    "timestamp": 7
                }),
            ),
            (
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: "c1".into(),
                    tool_name: "bash".into(),
                    content: "ok".into(),
                    is_error: false,
                    timestamp: 8,
                    duration_ms: Some(4),
                }),
                json!({
                    "role": "toolResult", "toolCallId": "c1", "toolName": "bash", "content": "ok",
                    "isError": false, "timestamp": 8, "durationMs": 4
                }),
            ),
        ];
        for (message, expected) in cases {
            assert_eq!(serde_json::to_value(&message).unwrap(), expected);
            assert_eq!(serde_json::from_value::<Message>(expected).unwrap(), message);
        }
    }

    #[test]
    fn continuation_kind_round_trips_and_survives_text_edits() {
        let message = Message::continuation("继续");
        let json = serde_json::to_value(&message).unwrap();
        assert_eq!(json["kind"], "continueAnswer");
        assert_eq!(serde_json::from_value::<Message>(json).unwrap(), message);
        assert!(
            matches!(message.with_replaced_text("接着说"), Message::User { kind: UserMessageKind::ContinueAnswer, content, .. } if content == "接着说")
        );
        let legacy: Message =
            serde_json::from_value(json!({"role": "user", "content": "old", "timestamp": 1})).unwrap();
        assert!(matches!(
            legacy,
            Message::User {
                kind: UserMessageKind::Prompt,
                ..
            }
        ));
    }

    #[test]
    fn text_and_tool_calls_read_the_blocks_in_order() {
        let message = assistant();
        assert_eq!(message.text(), "Checking.");
        let ids: Vec<String> = message.tool_calls().into_iter().map(|call| call.id).collect();
        assert_eq!(ids, ["c1"]);
    }

    #[test]
    fn usage_add_sums_every_count() {
        let mut total = assistant().usage;
        total.add(&assistant().usage);
        assert_eq!(
            total,
            Usage {
                input: 6,
                output: 4,
                cache_read: 2,
                total_tokens: 10
            }
        );
    }

    #[test]
    fn usage_totals_saturate_instead_of_overflowing() {
        let mut total = Usage {
            input: u64::MAX - 1,
            output: 1,
            cache_read: 0,
            total_tokens: u64::MAX,
        };
        total.add(&Usage {
            input: 5,
            output: u64::MAX,
            cache_read: 3,
            total_tokens: 1,
        });
        assert_eq!(
            total,
            Usage {
                input: u64::MAX,
                output: u64::MAX,
                cache_read: 3,
                total_tokens: u64::MAX
            }
        );
    }
}
