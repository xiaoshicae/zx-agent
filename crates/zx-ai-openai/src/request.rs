//! The JSON body of a Chat Completions request, the conversion options recorded with each
//! request, and the body rebuilt from them for the request audit (pi: transform-messages.ts).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use zx_ai::{AssistantMessage, ContentBlock, Message, Reasoning, Request, SystemPrompt};

use crate::accumulate::REASONING_FIELDS;
use crate::config::{BodyOptions, ReasoningFormat, SystemUpdates};

/// The name of this conversion in the recorded options.
const FORMAT: &str = "openai-chat-completions";
/// The version of this conversion in the recorded options, the only one replayed.
const VERSION: u32 = 2;

/// What is recorded with each request: which conversion turned the transcript into a body,
/// then its options, side by side in one object.
#[derive(Serialize)]
struct Record<'a> {
    format: &'a str,
    version: u32,
    #[serde(flatten)]
    options: &'a BodyOptions,
}

/// The conversion a record names. Read apart from the options, which refuse a field they do
/// not know: serde cannot do that for a flattened value.
#[derive(Deserialize)]
struct Conversion {
    format: String,
    version: u32,
}

/// `options` as recorded with each request.
pub(crate) fn request_options(options: &BodyOptions) -> Value {
    let record = Record {
        format: FORMAT,
        version: VERSION,
        options,
    };
    serde_json::to_value(record).expect("strings, numbers, booleans and a JSON map serialize")
}

/// Rebuild a Chat Completions JSON body from a persisted RequestConfig and Request.
/// Credentials, URLs and HTTP headers are intentionally absent. Rejects an unknown option
/// format, any version but the current one and an option it does not know, rather than
/// silently applying a different conversion policy.
pub fn replay_body(request: &Request, provider: &str, options: &Value) -> anyhow::Result<Value> {
    let Conversion { format, version } = Conversion::deserialize(options)?;
    anyhow::ensure!(format == FORMAT, "unsupported request conversion format");
    anyhow::ensure!(
        version == VERSION,
        "request conversion version {version} is not supported; only version {VERSION} can be replayed"
    );
    let mut options = options.clone();
    if let Some(recorded) = options.as_object_mut() {
        recorded.remove("format");
        recorded.remove("version");
    }
    let options: BodyOptions = serde_json::from_value(options)?;
    options.check()?;
    Ok(build_body(request, &options, provider))
}

/// The JSON body of a streaming Chat Completions request to `provider`, the name its
/// answers are recorded under.
pub(crate) fn build_body(request: &Request, options: &BodyOptions, provider: &str) -> Value {
    let messages = convert_messages(&request.messages, options, provider, &request.model);

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
        body[options.max_tokens_field.to_string().as_str()] = json!(max_tokens);
    }
    if options.prompt_cache_key
        && let Some(key) = &request.cache_key
    {
        body["prompt_cache_key"] = json!(key);
    }
    // Checked when the provider is built: none replaces a field set above.
    for (field, value) in &options.extra_fields {
        body[field.as_str()] = value.clone();
    }
    // After the extra fields: a level chosen for this request wins over a fixed one.
    if let Some(reasoning) = request.reasoning {
        apply_reasoning(&mut body, reasoning, options.reasoning_format);
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
/// Reasoning is sent back only to the `provider` and `model` that produced it. System
/// messages follow `options.system_updates`.
fn convert_messages(messages: &[Message], options: &BodyOptions, provider: &str, model: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    let inline = options.system_updates == SystemUpdates::Inline;
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
    if options.fill_missing_reasoning {
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
/// some answers carry reasoning (the provider example in docs/CONFIGURATION.md quotes such a
/// rejection).
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
