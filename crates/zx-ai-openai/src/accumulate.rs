//! The assistant message, built from the chunks of a streamed response.

use std::collections::HashMap;

use serde_json::{Value, json};
use zx_ai::text::prefix_noted;
use zx_ai::{AssistantMessage, ContentBlock, Delta, ProviderError, StopReason, ToolCall, Usage};

/// Response fields that carry reasoning text: `reasoning_content` (DeepSeek, vLLM) or
/// `reasoning` (OpenRouter, Ollama). The first one present wins; replay uses the same name.
pub(crate) const REASONING_FIELDS: [&str; 2] = ["reasoning_content", "reasoning"];

/// The most of an invalid argument text that its error keeps: enough for the model to see
/// what it sent, bounded so the session and the next request do not grow with it.
const MAX_RECEIVED_ARGUMENTS: usize = 2048;

/// One block of the answer as it arrives.
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
    /// The first `id` a chunk carried.
    response_id: Option<String>,
    /// The first `model` a chunk named, when it is not the one requested.
    response_model: Option<String>,
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
            response_id: None,
            response_model: None,
        }
    }

    /// Take in one chunk, as the deltas it carries. Error chunks are the caller's to notice.
    pub(crate) fn apply(&mut self, chunk: &Value) -> Vec<Delta> {
        if self.response_id.is_none()
            && let Some(id) = chunk.get("id").and_then(Value::as_str).filter(|id| !id.is_empty())
        {
            self.response_id = Some(id.to_string());
        }
        if self.response_model.is_none()
            && let Some(model) = chunk.get("model").and_then(Value::as_str)
            && !model.is_empty()
            && model != self.model
        {
            self.response_model = Some(model.to_string());
        }
        if let Some(usage) = chunk.get("usage").filter(|usage| usage.is_object()) {
            self.apply_usage(usage);
        }

        let mut deltas = Vec::new();
        let Some(choice) = chunk.get("choices").and_then(|choices| choices.get(0)) else {
            return deltas;
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
                self.append(Block::Thinking {
                    text: text.to_string(),
                    field,
                });
                deltas.push(Delta::Thinking { text: text.to_string() });
            }
            if let Some(text) = delta
                .get("content")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                self.append(Block::Text(text.to_string()));
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
        deltas
    }

    /// Add a text or thinking block: to the last block when it is of the same kind,
    /// otherwise as a new block.
    fn append(&mut self, block: Block) {
        match (self.blocks.last_mut(), block) {
            (Some(Block::Text(existing)), Block::Text(text))
            | (Some(Block::Thinking { text: existing, .. }), Block::Thinking { text, .. }) => existing.push_str(&text),
            (_, block) => self.blocks.push(block),
        }
    }

    fn apply_tool_call(&mut self, position: usize, call: &Value, deltas: &mut Vec<Delta>) {
        let index = call.get("index").and_then(Value::as_u64).unwrap_or(position as u64);
        let function = call.get("function");
        let name = function
            .and_then(|function| function.get("name"))
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
                    .map_or_else(|| format!("call_{index}"), str::to_string);
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
            .and_then(|function| function.get("arguments"))
            .and_then(Value::as_str)
            .filter(|fragment| !fragment.is_empty())
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
        // `prompt_cache_hit_tokens`.
        self.usage.cache_read = usage
            .get("prompt_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_u64)
            .or_else(|| usage.get("prompt_cache_hit_tokens").and_then(Value::as_u64))
            .unwrap_or(0);
        // Reported by a few servers only (pi: `cache_write_tokens`, `reasoning_tokens`).
        self.usage.cache_write = usage
            .get("prompt_tokens_details")
            .and_then(|details| details.get("cache_write_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        self.usage.reasoning = usage
            .get("completion_tokens_details")
            .and_then(|details| details.get("reasoning_tokens"))
            .and_then(Value::as_u64)
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
                    ContentBlock::ToolCall(ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments,
                        arguments_error,
                    })
                }
            })
            .collect()
    }

    /// The complete message, from the finish reason: only the known endings succeed, and any
    /// other value means the server interrupted or rejected the answer. Fails on a stream
    /// that ended without a finish reason.
    pub(crate) fn finish(&self) -> Result<AssistantMessage, String> {
        let Some(reason) = self.finish_reason.as_deref() else {
            return Err("stream ended without a finish_reason".to_string());
        };
        let has_tool_calls = self.blocks.iter().any(|block| matches!(block, Block::Tool { .. }));
        let (stop_reason, error) = match reason {
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
        // Prefixed so that an ending reason never shares a name with an error code that is
        // retried, such as "timeout": an unknown ending is not retried.
        let details = error.is_some().then(|| ProviderError::Provider {
            code: Some(format!("finish_reason:{reason}")),
        });
        Ok(self.message(stop_reason, error, details))
    }

    /// The message so far, ended by `stop_reason` with `error` and its `details`.
    pub(crate) fn failure(
        &self,
        stop_reason: StopReason,
        error: impl Into<String>,
        details: Option<ProviderError>,
    ) -> AssistantMessage {
        self.message(stop_reason, Some(error.into()), details)
    }

    fn message(
        &self,
        stop_reason: StopReason,
        error_message: Option<String>,
        provider_error: Option<ProviderError>,
    ) -> AssistantMessage {
        AssistantMessage {
            content: self.content(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            usage: self.usage.clone(),
            stop_reason,
            error_message,
            provider_error,
            timestamp: self.timestamp,
            duration_ms: None,
            response_id: self.response_id.clone(),
            response_model: self.response_model.clone(),
        }
    }
}

fn parse_arguments(raw: &str) -> (Value, Option<String>) {
    if raw.trim().is_empty() {
        return (json!({}), None);
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(value) => (value, None),
        Err(e) => {
            let received = prefix_noted(raw, MAX_RECEIVED_ARGUMENTS, |_, _| "…".to_string());
            (
                json!({}),
                Some(format!(
                    "arguments are not valid JSON: {e}\n\nReceived arguments:\n{received}"
                )),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply_all(acc: &mut Accumulator, chunks: &[Value]) -> Vec<Delta> {
        chunks.iter().flat_map(|chunk| acc.apply(chunk)).collect()
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
                total_tokens: 17,
                ..Usage::default()
            }
        );
    }

    #[test]
    fn reports_invalid_tool_arguments_and_missing_finish_reason() {
        let mut acc = Accumulator::new("p", "m", 1);
        acc.apply(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"bash","arguments":"{\"command\":"}}]}}]}));
        assert!(acc.finish().is_err());
        let failed = acc.failure(StopReason::Error, "cut off", None);
        assert!(failed.tool_calls()[0].arguments_error.is_some());
    }

    #[test]
    fn invalid_tool_arguments_keep_what_the_model_sent() {
        let raw = "{\"command\":\"echo \"quoted\" text\"}";
        let (arguments, error) = parse_arguments(raw);

        assert_eq!(arguments, json!({}));
        let error = error.unwrap();
        assert!(error.starts_with("arguments are not valid JSON: "), "{error}");
        assert!(error.ends_with(&format!("\n\nReceived arguments:\n{raw}")), "{error}");

        // A long one is cut at a character boundary, so the error stays bounded.
        let long = format!("{{\"text\":\"{}", "界".repeat(2000));
        let error = parse_arguments(&long).1.unwrap();
        assert!(error.len() < MAX_RECEIVED_ARGUMENTS + 200, "{}", error.len());
        assert!(error.ends_with("…"), "{error}");
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
            args.iter().map(|delta| delta["id"].as_str()).collect::<Vec<_>>(),
            [Some("a"), Some("b"), Some("a"), Some("b")]
        );
    }

    #[test]
    fn stop_with_tool_calls_is_tool_use() {
        let mut acc = Accumulator::new("p", "m", 1);
        acc.apply(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"x","arguments":"{}"}}]},"finish_reason":"stop"}]}));
        assert_eq!(acc.finish().unwrap().stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn only_known_finish_reasons_succeed() {
        let finish = |reason: &str| {
            let mut acc = Accumulator::new("p", "m", 1);
            acc.apply(&json!({"choices":[{"delta":{"content":"x"},"finish_reason":reason}]}));
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
                    code: Some(format!("finish_reason:{reason}"))
                })
            );
            assert!(message.error_message.is_some(), "{reason}");
        }
    }

    #[test]
    fn an_unknown_finish_reason_is_not_retried_whatever_its_name() {
        for reason in ["timeout", "server_error", "rate_limit_exceeded", "network_error"] {
            let mut acc = Accumulator::new("p", "m", 1);
            acc.apply(&json!({"choices":[{"delta":{"content":"x"},"finish_reason":reason}]}));
            let message = acc.finish().unwrap();

            assert_eq!(message.stop_reason, StopReason::Error, "{reason}");
            assert!(!zx_ai::retry::is_retryable(&message), "{reason}");
        }
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
        );
        assert_eq!(acc.usage.cache_read, 6);
    }

    #[test]
    fn a_missing_total_is_computed_without_overflowing() {
        let mut acc = Accumulator::new("p", "m", 1);
        acc.apply(&json!({"choices":[],"usage":{"prompt_tokens":u64::MAX,"completion_tokens":1}}));
        assert_eq!(acc.usage.total_tokens, u64::MAX);
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
        let expected = ContentBlock::Thinking {
            thinking: "Thinking.".into(),
            signature: Some("reasoning_content".into()),
        };
        assert_eq!(acc.finish().unwrap().content[0], expected);
    }

    #[test]
    fn usage_counts_cache_writes_and_reasoning_tokens() {
        let mut acc = Accumulator::new("p", "m", 0);
        acc.apply(&json!({"choices": [{"index": 0, "delta": {"content": "x"}, "finish_reason": "stop"}]}));
        acc.apply(&json!({"choices": [], "usage": {
            "prompt_tokens": 20, "completion_tokens": 7, "total_tokens": 27,
            "prompt_tokens_details": {"cached_tokens": 4, "cache_write_tokens": 6},
            "completion_tokens_details": {"reasoning_tokens": 3}
        }}));
        let usage = acc.finish().unwrap().usage;
        assert_eq!((usage.cache_read, usage.cache_write, usage.reasoning), (4, 6, 3));
        assert_eq!((usage.input, usage.output, usage.total_tokens), (20, 7, 27));
    }

    #[test]
    fn the_response_id_and_a_differing_model_name_are_kept() {
        let mut acc = Accumulator::new("p", "gpt-x", 0);
        acc.apply(&json!({"id": "chatcmpl-1", "model": "gpt-x-2026-01-01", "choices": [{"index": 0, "delta": {"content": "a"}}]}));
        acc.apply(&json!({"id": "chatcmpl-2", "model": "other", "choices": [{"index": 0, "delta": {"content": "b"}, "finish_reason": "stop"}]}));
        let message = acc.finish().unwrap();
        assert_eq!(
            message.response_id.as_deref(),
            Some("chatcmpl-1"),
            "the first id is kept"
        );
        assert_eq!(message.response_model.as_deref(), Some("gpt-x-2026-01-01"));

        let mut same = Accumulator::new("p", "gpt-x", 0);
        same.apply(&json!({"id": "", "model": "gpt-x", "choices": [{"index": 0, "delta": {"content": "a"}, "finish_reason": "stop"}]}));
        let message = same.finish().unwrap();
        assert_eq!((message.response_id, message.response_model), (None, None));
        let failed = Accumulator::new("p", "gpt-x", 0).failure(StopReason::Error, "boom", None);
        assert_eq!(failed.response_id, None);
    }
}
