//! The JSON body of a Chat Completions request, the conversion options recorded with each
//! request, and the body rebuilt from them for the request audit (pi: transform-messages.ts).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use zx_ai::{AssistantMessage, ContentBlock, Message, Reasoning, Request, SystemPrompt};

use crate::accumulate::REASONING_FIELDS;
use crate::config::{OpenAiConfig, ReasoningFormat, SystemUpdates, check_extra_fields};

/// The name of this conversion in the recorded options.
const FORMAT: &str = "openai-chat-completions";
/// The version of this conversion in the recorded options, the only one replayed.
const VERSION: u32 = 2;

/// The conversion options recorded with each request: the policy that turns a transcript
/// into a body, and nothing secret. Fields absent unless set stay out of the record.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Options {
    format: String,
    version: u32,
    system_updates: String,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    extra_fields: Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fill_missing_reasoning: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<bool>,
}

/// The conversion options of `config`, as recorded with each request.
pub(crate) fn request_options(config: &OpenAiConfig) -> Value {
    let options = Options {
        format: FORMAT.to_string(),
        version: VERSION,
        system_updates: config.system_updates.as_str().to_string(),
        extra_fields: config.extra_fields.clone(),
        fill_missing_reasoning: config.fill_missing_reasoning.then_some(true),
        reasoning_format: (config.reasoning_format != ReasoningFormat::default())
            .then(|| config.reasoning_format.as_str().to_string()),
        prompt_cache_key: config.prompt_cache_key.then_some(true),
    };
    serde_json::to_value(options).expect("strings, numbers, booleans and a JSON map serialize")
}

/// Rebuild a Chat Completions JSON body from a persisted RequestConfig and Request.
/// Credentials, URLs and HTTP headers are intentionally absent. Rejects an unknown option
/// format and any version but the current one, rather than silently applying a different
/// conversion policy.
pub fn replay_body(request: &Request, provider: &str, options: &Value) -> anyhow::Result<Value> {
    let options: Options = serde_json::from_value(options.clone())?;
    anyhow::ensure!(options.format == FORMAT, "unsupported request conversion format");
    anyhow::ensure!(
        options.version == VERSION,
        "request conversion version {} is not supported; only version {VERSION} can be replayed",
        options.version
    );
    let fill_missing_reasoning = options.fill_missing_reasoning.unwrap_or(false);
    let system_updates: SystemUpdates = options.system_updates.parse().map_err(anyhow::Error::msg)?;
    let prompt_cache_key = options.prompt_cache_key.unwrap_or(false);
    check_extra_fields(&options.extra_fields, prompt_cache_key)?;
    let reasoning_format = match options.reasoning_format.as_deref() {
        None => ReasoningFormat::Effort,
        Some(name) => name.parse().map_err(anyhow::Error::msg)?,
    };
    Ok(build_body(
        request,
        &OpenAiConfig {
            provider_name: provider.into(),
            system_updates,
            extra_fields: options.extra_fields,
            fill_missing_reasoning,
            reasoning_format,
            prompt_cache_key,
            ..OpenAiConfig::default()
        },
    ))
}

/// The JSON body of a streaming Chat Completions request.
pub(crate) fn build_body(request: &Request, config: &OpenAiConfig) -> Value {
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
    if config.prompt_cache_key
        && let Some(key) = &request.cache_key
    {
        body["prompt_cache_key"] = json!(key);
    }
    // Checked when the provider is built: none replaces a field set above.
    for (field, value) in &config.extra_fields {
        body[field.as_str()] = value.clone();
    }
    // After the extra fields: a level chosen for this request wins over a fixed one.
    if let Some(reasoning) = request.reasoning {
        apply_reasoning(&mut body, reasoning, config.reasoning_format);
    }
    body
}

/// Write `reasoning` into `body` as `format` says.
fn apply_reasoning(body: &mut Value, reasoning: Reasoning, format: ReasoningFormat) {
    let level = reasoning.as_str();
    match (format, reasoning) {
        (ReasoningFormat::Effort, Reasoning::Off) => {}
        (ReasoningFormat::Effort, _) => body["reasoning_effort"] = json!(level),
        (ReasoningFormat::Thinking, Reasoning::Off) => body["thinking"] = json!({"type": "disabled"}),
        (ReasoningFormat::Thinking, _) => {
            body["thinking"] = json!({"type": "enabled"});
            body["reasoning_effort"] = json!(level);
        }
        (ReasoningFormat::ReasoningObject, Reasoning::Off) => body["reasoning"] = json!({"effort": "none"}),
        (ReasoningFormat::ReasoningObject, _) => body["reasoning"] = json!({"effort": level}),
    }
}

/// Convert the transcript, keeping tool calls and results paired.
///
/// Providers reject a tool call without a result and a result without its call. A run
/// cancelled between them leaves such gaps, so missing results get a placeholder and
/// unmatched results are dropped.
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
                if let Some((entry, calls)) = assistant_entry(assistant, provider, model) {
                    pending = calls;
                    out.push(entry);
                }
            }
        }
    }
    flush_missing_results(&mut out, &mut pending);
    if config.fill_missing_reasoning {
        fill_missing_reasoning(&mut out);
    }
    out
}

/// The body entry of an assistant message and the ids of the tool calls it makes, or nothing
/// for an empty failed or aborted attempt, which carries nothing for the model.
fn assistant_entry(assistant: &AssistantMessage, provider: &str, model: &str) -> Option<(Value, Vec<String>)> {
    let text = assistant.text();
    let calls = assistant.tool_calls();
    if text.is_empty() && calls.is_empty() {
        return None;
    }
    let mut entry = json!({
        "role": "assistant",
        "content": if text.is_empty() { Value::Null } else { Value::String(text) },
    });
    if let Some((field, reasoning)) = replayed_reasoning(assistant, provider, model) {
        entry[field] = Value::String(reasoning);
    }
    if !calls.is_empty() {
        entry["tool_calls"] = calls
            .iter()
            .map(|call| {
                json!({
                    "id": call.id,
                    "type": "function",
                    "function": { "name": call.name, "arguments": call.arguments.to_string() },
                })
            })
            .collect();
    }
    Some((entry, calls.into_iter().map(|call| call.id).collect()))
}

/// The reasoning to send back with an assistant message: the field it arrived in and its
/// text. Reasoning is meaningful only to the model that produced it.
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

/// Give every assistant message the `reasoning_content` field once one of them carries it,
/// empty where there was none: thinking-mode servers can reject a history in which only
/// some answers carry reasoning (what such a server answers, and what was observed, is in
/// the README's notes on that service).
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

#[cfg(test)]
mod tests;
