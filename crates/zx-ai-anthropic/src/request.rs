//! The JSON body of a Messages request, and the conversion options recorded with each request
//! (pi: anthropic-messages.ts).

use serde::Serialize;
use serde_json::{Value, json};
use zx_ai::{AssistantMessage, ContentBlock, Message, Reasoning, Request, SystemPrompt, ToolDef};

use crate::config::{BodyOptions, SystemUpdates};

/// The name of this conversion in the recorded options.
const FORMAT: &str = "anthropic-messages";
/// The version of this conversion in the recorded options.
const VERSION: u32 = 1;

/// The output limit of a request that sets none, which the API requires: room for long
/// answers on current models, whose responses stream.
pub(crate) const DEFAULT_MAX_TOKENS: u32 = 64_000;

/// What a thinking block the API redacted is recorded with in place of its signature: this
/// marker, then the block's encrypted data. No signature starts with it.
pub(crate) const REDACTED: &str = "redacted:";

/// The result sent for a tool call that has none, such as one a cancelled run left behind:
/// the API refuses a call without a result.
const NO_RESULT: &str = "No result provided";

/// What is recorded with each request: which conversion turned the transcript into a body,
/// then its options, side by side in one object.
#[derive(Serialize)]
struct Record<'a> {
    format: &'a str,
    version: u32,
    #[serde(flatten)]
    options: &'a BodyOptions,
}

/// `options` as recorded with each request.
pub(crate) fn request_options(options: &BodyOptions) -> Value {
    let record = Record {
        format: FORMAT,
        version: VERSION,
        options,
    };
    serde_json::to_value(record).expect("strings and a JSON map serialize")
}

/// The JSON body of a streaming Messages request to `provider`, the name its answers are
/// recorded under. The system prompt, the tools and the end of the conversation are cache
/// breakpoints, so that a conversation's next request reads what this one wrote.
pub(crate) fn build_body(request: &Request, options: &BodyOptions, provider: &str) -> Value {
    let (system, messages) = convert_messages(&request.messages, options.system_updates, provider, &request.model);
    let mut body = json!({
        "model": request.model,
        "max_tokens": request.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        "messages": messages,
        "stream": true,
    });
    if let Some(system) = system {
        body["system"] = json!([{ "type": "text", "text": system, "cache_control": cache() }]);
    }
    if !request.tools.is_empty() {
        body["tools"] = tools(&request.tools);
    }
    // Checked when the provider is built: none replaces a field set above.
    for (field, value) in &options.extra_fields {
        body[field.as_str()] = value.clone();
    }
    // After the extra fields: a level chosen for this request wins over a fixed one.
    if let Some(reasoning) = request.reasoning {
        apply_reasoning(&mut body, reasoning);
    }
    body
}

/// The cache breakpoint of this provider: the API's default lifetime.
fn cache() -> Value {
    json!({ "type": "ephemeral" })
}

/// The tool definitions, the last one a cache breakpoint.
fn tools(tools: &[ToolDef]) -> Value {
    let last = tools.len().saturating_sub(1);
    tools
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            let mut definition = json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.parameters,
            });
            if index == last {
                definition["cache_control"] = cache();
            }
            definition
        })
        .collect()
}

/// Write `reasoning` into `body`: adaptive thinking at an effort, with the thinking shown as a
/// summary, or thinking turned off. Minimal is the API's lowest effort, `low`. An
/// `output_config` among the extra fields keeps its other settings.
fn apply_reasoning(body: &mut Value, reasoning: Reasoning) {
    let effort = match reasoning {
        Reasoning::Off => {
            body["thinking"] = json!({ "type": "disabled" });
            return;
        }
        Reasoning::Minimal | Reasoning::Low => "low",
        Reasoning::Medium => "medium",
        Reasoning::High => "high",
        Reasoning::Xhigh => "xhigh",
    };
    body["thinking"] = json!({ "type": "adaptive", "display": "summarized" });
    if !body["output_config"].is_object() {
        body["output_config"] = json!({});
    }
    body["output_config"]["effort"] = json!(effort);
}

/// The conversation's turns as the body's `messages` are built: a block joins the turn before
/// it when the role is the same, since the API takes alternating turns.
#[derive(Default)]
struct Turns(Vec<Value>);

impl Turns {
    fn push(&mut self, role: &str, block: Value) {
        if role != "system"
            && let Some(last) = self.0.last_mut()
            && last["role"] == role
            && let Some(content) = last["content"].as_array_mut()
        {
            content.push(block);
            return;
        }
        self.0.push(json!({ "role": role, "content": [block] }));
    }

    /// A result for each call of `pending` that has none.
    fn close_calls(&mut self, pending: &mut Vec<String>) {
        for id in pending.drain(..) {
            self.push("user", tool_result(&id, NO_RESULT, true));
        }
    }

    /// The system prompt changes held back, as one system turn, which the API takes after a
    /// user turn and before an answer.
    fn release(&mut self, held: &mut Vec<String>) {
        if !held.is_empty() {
            self.push("system", text(&held.join("\n\n")));
            held.clear();
        }
    }

    /// Make the end of the conversation a cache breakpoint. Only a user or system turn ends
    /// with a block that takes one.
    fn mark_end_cached(&mut self) {
        if let Some(last) = self.0.last_mut()
            && last["role"] != "assistant"
            && let Some(block) = last["content"].as_array_mut().and_then(|content| content.last_mut())
        {
            block["cache_control"] = cache();
        }
    }
}

/// The top-level system prompt and the body's `messages`, with tool calls and results
/// paired: the API refuses a call without a result and a result without its call, which a
/// run cancelled between them leaves, so a missing result gets a placeholder and an
/// unmatched one is left out.
///
/// The first system prompt goes to the top level. Later changes follow `updates`: folded
/// into it, or sent as system turns, held back until the next answer of the model, since the
/// API takes a system turn only after a user turn and a tool result must follow its call.
///
/// Thinking is sent back only to the `provider` and `model` that produced it: its signature
/// is valid for that model alone (pi: `transformMessages`).
fn convert_messages(
    messages: &[Message],
    updates: SystemUpdates,
    provider: &str,
    model: &str,
) -> (Option<String>, Vec<Value>) {
    let inline = updates == SystemUpdates::Inline;
    let mut system = None;
    if !inline {
        let prompt = SystemPrompt::from_transcript(messages);
        system = (!prompt.is_empty()).then(|| prompt.render());
    }
    let mut turns = Turns::default();
    // Inline mode: the prompt so far, and the changes waiting for their place.
    let mut prompt = SystemPrompt::default();
    let mut held = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    for message in messages {
        match message {
            Message::System(_) if !inline => {}
            Message::System(change) => {
                let first = prompt.is_empty();
                prompt.apply(&change.sections);
                if first {
                    system = (!prompt.is_empty()).then(|| prompt.render());
                } else {
                    let text = change.render_update();
                    if !text.is_empty() {
                        held.push(text);
                    }
                }
            }
            Message::User { content, .. } => {
                turns.close_calls(&mut pending);
                if !content.trim().is_empty() {
                    turns.push("user", text(content));
                }
            }
            Message::ToolResult(result) => {
                let Some(position) = pending.iter().position(|id| *id == result.tool_call_id) else {
                    continue;
                };
                pending.remove(position);
                let content = if result.content.is_empty() {
                    "(no output)"
                } else {
                    result.content.as_str()
                };
                turns.push("user", tool_result(&result.tool_call_id, content, result.is_error));
            }
            Message::Assistant(answer) => {
                turns.close_calls(&mut pending);
                let (blocks, calls) = answer_blocks(answer, provider, model);
                if blocks.is_empty() {
                    continue;
                }
                turns.release(&mut held);
                for block in blocks {
                    turns.push("assistant", block);
                }
                pending = calls;
            }
        }
    }
    turns.close_calls(&mut pending);
    turns.release(&mut held);
    turns.mark_end_cached();
    (system, turns.0)
}

/// The blocks of an answer as the API takes them back, and the ids of the tool calls it
/// makes; none for an empty failed or aborted attempt, which carries nothing for the model.
/// Thinking goes back unchanged with its signature to the `provider` and `model` that
/// produced it; a block cut off before its signature arrived cannot go back.
fn answer_blocks(answer: &AssistantMessage, provider: &str, model: &str) -> (Vec<Value>, Vec<String>) {
    let thinking_goes_back = answer.provider == provider && answer.model == model;
    let mut blocks = Vec::new();
    let mut calls = Vec::new();
    for block in &answer.content {
        match block {
            ContentBlock::Text { text: written } => {
                if !written.trim().is_empty() {
                    blocks.push(text(written));
                }
            }
            ContentBlock::Thinking { thinking, signature } => {
                let Some(signature) = signature.as_deref().filter(|_| thinking_goes_back) else {
                    continue;
                };
                if let Some(data) = signature.strip_prefix(REDACTED) {
                    blocks.push(json!({ "type": "redacted_thinking", "data": data }));
                } else if !signature.is_empty() {
                    blocks.push(json!({ "type": "thinking", "thinking": thinking, "signature": signature }));
                }
            }
            ContentBlock::ToolCall(call) => {
                // The API takes an object; arguments that were not one were refused already.
                let input = if call.arguments.is_object() {
                    call.arguments.clone()
                } else {
                    json!({})
                };
                blocks.push(json!({ "type": "tool_use", "id": tool_id(&call.id), "name": call.name, "input": input }));
                calls.push(call.id.clone());
            }
        }
    }
    (blocks, calls)
}

fn text(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

fn tool_result(id: &str, content: &str, is_error: bool) -> Value {
    json!({ "type": "tool_result", "tool_use_id": tool_id(id), "content": content, "is_error": is_error })
}

/// A tool call's id in the form the API takes, letters, digits, `_` and `-`, at most 64
/// characters: the ids of another provider's answers may hold other characters (pi:
/// `normalizeToolCallId`).
fn tool_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

/// Leave out the thinking blocks of the body's messages from the one at `from` on, for a
/// request the API refused because one of them is bound to a different conversation.
pub(crate) fn drop_thinking_from(body: &mut Value, from: usize) {
    let Some(messages) = body["messages"].as_array_mut() else {
        return;
    };
    for message in messages.iter_mut().skip(from) {
        if let Some(content) = message["content"].as_array_mut() {
            content.retain(|block| !matches!(block["type"].as_str(), Some("thinking" | "redacted_thinking")));
        }
    }
    // A turn that held thinking alone is left without content, which the API refuses.
    messages.retain(|message| message["content"].as_array().is_none_or(|content| !content.is_empty()));
}

#[cfg(test)]
mod tests;
