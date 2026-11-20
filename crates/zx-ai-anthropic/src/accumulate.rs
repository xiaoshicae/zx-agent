//! The message built from the events of a streamed response, and the deltas forwarded as they
//! arrive (pi: anthropic-messages.ts).

use std::collections::HashMap;

use serde_json::Value;
use zx_ai::{
    AssistantMessage, ContentBlock, Delta, InputTransformation, ProviderError, ProviderErrorKind, StopReason, ToolCall,
    Usage,
};
use zx_ai_http::Failure;
use zx_ai_http::stream::Answer;

use crate::failure::provider_failure;
use crate::request::REDACTED;

/// One block of the answer as it streams.
enum Block {
    Text(String),
    Thinking {
        text: String,
        signature: String,
    },
    /// A thinking block the API redacted: its encrypted data, sent back as it came.
    Redacted(String),
    Tool {
        id: String,
        name: String,
        arguments: String,
    },
}

/// The answer as far as its events have come.
pub(crate) struct Accumulator {
    provider: String,
    model: String,
    timestamp: u64,
    blocks: Vec<Block>,
    /// Where each block the response opened is in `blocks`, by the index the API gave it. A
    /// block of a kind this provider does not keep, such as a server tool's, has none.
    positions: HashMap<u64, usize>,
    usage: Usage,
    /// The prompt tokens outside the cache, which the API counts apart from the cached ones.
    fresh_input: u64,
    /// The `stop_reason` of the response, with its `stop_details`.
    stop: Option<(String, Value)>,
    response_id: Option<String>,
    response_model: Option<String>,
    /// What the API changed in the request, as the last event that listed it says.
    input_transformations: Vec<InputTransformation>,
}

impl Accumulator {
    pub(crate) fn new(provider: &str, model: &str, timestamp: u64) -> Self {
        Self {
            provider: provider.to_string(),
            model: model.to_string(),
            timestamp,
            blocks: Vec::new(),
            positions: HashMap::new(),
            usage: Usage::default(),
            fresh_input: 0,
            stop: None,
            response_id: None,
            response_model: None,
            input_transformations: Vec::new(),
        }
    }

    /// Apply one event of the stream, returning the deltas to forward. An `error` event fails
    /// the response; events of types added to the API since are passed over.
    pub(crate) fn apply(&mut self, event: &Value) -> Result<Vec<Delta>, Failure> {
        let mut deltas = Vec::new();
        match event["type"].as_str() {
            Some("message_start") => self.start(&event["message"]),
            Some("content_block_start") => self.open(event, &mut deltas),
            Some("content_block_delta") => self.extend(event, &mut deltas),
            Some("message_delta") => {
                if let Some(reason) = event["delta"]["stop_reason"].as_str() {
                    self.stop = Some((reason.to_string(), event["delta"]["stop_details"].clone()));
                }
                self.apply_usage(&event["usage"]);
                self.apply_transformations(&event["input_transformations"]);
            }
            Some("error") => return Err(provider_failure(&event["error"])),
            // `content_block_stop`, `message_stop` and `ping` change nothing here.
            _ => {}
        }
        Ok(deltas)
    }

    fn start(&mut self, message: &Value) {
        self.response_id = message["id"].as_str().filter(|id| !id.is_empty()).map(str::to_string);
        self.response_model = message["model"]
            .as_str()
            .filter(|model| !model.is_empty() && *model != self.model)
            .map(str::to_string);
        self.apply_usage(&message["usage"]);
        self.apply_transformations(&message["input_transformations"]);
    }

    /// Read the changes to the request that `listed` reports. The API lists them at the start
    /// and, after a fallback to another model during the response, again in the last
    /// `message_delta`, whose list replaces the first; a value that is not a list changes
    /// nothing. An entry without a `type` says nothing and is passed over.
    fn apply_transformations(&mut self, listed: &Value) {
        let Some(entries) = listed.as_array() else {
            return;
        };
        let text = |entry: &Value, field: &str| entry[field].as_str().map(str::to_string);
        self.input_transformations = entries
            .iter()
            .filter_map(|entry| {
                Some(InputTransformation {
                    kind: text(entry, "type")?,
                    path: text(entry, "path"),
                    reason: text(entry, "reason"),
                })
            })
            .collect();
    }

    fn open(&mut self, event: &Value, deltas: &mut Vec<Delta>) {
        let Some(index) = event["index"].as_u64() else {
            return;
        };
        let block = &event["content_block"];
        let text_of = |field: &str| block[field].as_str().unwrap_or_default().to_string();
        let opened = match block["type"].as_str() {
            Some("text") => {
                let text = text_of("text");
                if !text.is_empty() {
                    deltas.push(Delta::Text { text: text.clone() });
                }
                Block::Text(text)
            }
            Some("thinking") => {
                let text = text_of("thinking");
                if !text.is_empty() {
                    deltas.push(Delta::Thinking { text: text.clone() });
                }
                Block::Thinking {
                    text,
                    signature: text_of("signature"),
                }
            }
            Some("redacted_thinking") => Block::Redacted(text_of("data")),
            Some("tool_use") => {
                let (id, name) = (text_of("id"), text_of("name"));
                deltas.push(Delta::ToolCallStart {
                    id: id.clone(),
                    name: name.clone(),
                });
                Block::Tool {
                    id,
                    name,
                    arguments: String::new(),
                }
            }
            _ => return,
        };
        self.positions.insert(index, self.blocks.len());
        self.blocks.push(opened);
    }

    fn extend(&mut self, event: &Value, deltas: &mut Vec<Delta>) {
        let Some(block) = event["index"]
            .as_u64()
            .and_then(|index| self.positions.get(&index))
            .and_then(|&position| self.blocks.get_mut(position))
        else {
            return;
        };
        let delta = &event["delta"];
        let piece = |field: &str| delta[field].as_str().unwrap_or_default().to_string();
        match (block, delta["type"].as_str()) {
            (Block::Text(text), Some("text_delta")) => {
                let piece = piece("text");
                text.push_str(&piece);
                deltas.push(Delta::Text { text: piece });
            }
            (Block::Thinking { text, .. }, Some("thinking_delta")) => {
                let piece = piece("thinking");
                text.push_str(&piece);
                deltas.push(Delta::Thinking { text: piece });
            }
            (Block::Thinking { signature, .. }, Some("signature_delta")) => signature.push_str(&piece("signature")),
            (Block::Tool { id, arguments, .. }, Some("input_json_delta")) => {
                let piece = piece("partial_json");
                arguments.push_str(&piece);
                deltas.push(Delta::ToolCallArgs {
                    id: id.clone(),
                    text: piece,
                });
            }
            // Citations, and deltas of kinds added since.
            _ => {}
        }
    }

    /// Read the counts a usage object reports. A count it leaves out keeps its value: a
    /// `message_delta` may report the output alone.
    fn apply_usage(&mut self, usage: &Value) {
        let count = |key: &str| usage[key].as_u64();
        if let Some(fresh) = count("input_tokens") {
            self.fresh_input = fresh;
        }
        if let Some(read) = count("cache_read_input_tokens") {
            self.usage.cache_read = read;
        }
        if let Some(written) = count("cache_creation_input_tokens") {
            self.usage.cache_write = written;
        }
        if let Some(output) = count("output_tokens") {
            self.usage.output = output;
        }
        if let Some(thinking) = usage["output_tokens_details"]["thinking_tokens"].as_u64() {
            self.usage.reasoning = thinking;
        }
    }

    fn message(
        &self,
        stop_reason: StopReason,
        error_message: Option<String>,
        provider_error: Option<ProviderError>,
    ) -> AssistantMessage {
        let mut usage = self.usage.clone();
        usage.input = self
            .fresh_input
            .saturating_add(usage.cache_read)
            .saturating_add(usage.cache_write);
        usage.total_tokens = usage.input.saturating_add(usage.output);
        AssistantMessage {
            content: self.content(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            usage,
            stop_reason,
            error_message,
            provider_error,
            timestamp: self.timestamp,
            duration_ms: None,
            response_id: self.response_id.clone(),
            response_model: self.response_model.clone(),
            input_transformations: self.input_transformations.clone(),
        }
    }

    fn content(&self) -> Vec<ContentBlock> {
        self.blocks
            .iter()
            .map(|block| match block {
                Block::Text(text) => ContentBlock::Text { text: text.clone() },
                Block::Thinking { text, signature } => ContentBlock::Thinking {
                    thinking: text.clone(),
                    signature: (!signature.is_empty()).then(|| signature.clone()),
                },
                Block::Redacted(data) => ContentBlock::Thinking {
                    thinking: String::new(),
                    signature: Some(format!("{REDACTED}{data}")),
                },
                Block::Tool { id, name, arguments } => {
                    ContentBlock::ToolCall(ToolCall::streamed(id.clone(), name.clone(), arguments))
                }
            })
            .collect()
    }
}

impl Answer for Accumulator {
    /// The complete message, from the stop reason. A full context window ends it as a failure
    /// of the kind [`ProviderErrorKind::ContextOverflow`], since the conversation no longer
    /// fits and a continuation would not either; a refusal, and a reason this provider does
    /// not know, as one of the kind [`ProviderErrorKind::Refused`]; both with the reason as
    /// its code. Fails on a stream that ended without a stop reason.
    fn finish(&self) -> Result<AssistantMessage, String> {
        let Some((reason, details)) = &self.stop else {
            return Err("stream ended without a stop_reason".to_string());
        };
        let stop_reason = match reason.as_str() {
            "tool_use" => StopReason::ToolUse,
            // A paused turn ends only with server tools, which this provider does not declare.
            "end_turn" | "stop_sequence" | "pause_turn" => StopReason::Stop,
            "max_tokens" => StopReason::Length,
            other => {
                let (kind, error) = match other {
                    "model_context_window_exceeded" => (
                        ProviderErrorKind::ContextOverflow,
                        "the response filled the model's context window".to_string(),
                    ),
                    "refusal" => (ProviderErrorKind::Refused, refusal(details)),
                    _ => (
                        ProviderErrorKind::Refused,
                        format!("the API ended the response with stop_reason {other:?}"),
                    ),
                };
                let provider_error = ProviderError {
                    kind,
                    status: None,
                    code: Some(other.to_string()),
                    retry_after_ms: None,
                };
                return Ok(self.message(StopReason::Error, Some(error), Some(provider_error)));
            }
        };
        Ok(self.message(stop_reason, None, None))
    }

    fn failure(
        &self,
        stop_reason: StopReason,
        error: impl Into<String>,
        provider_error: Option<ProviderError>,
    ) -> AssistantMessage {
        self.message(stop_reason, Some(error.into()), provider_error)
    }
}

/// What the error of a refused request says: the category the API gave, and its explanation.
fn refusal(details: &Value) -> String {
    let mut text = "the model refused the request".to_string();
    if let Some(category) = details["category"].as_str() {
        text.push_str(&format!(" ({category})"));
    }
    if let Some(explanation) = details["explanation"].as_str().filter(|text| !text.is_empty()) {
        text.push_str(&format!(": {explanation}"));
    }
    text
}

#[cfg(test)]
mod tests;
