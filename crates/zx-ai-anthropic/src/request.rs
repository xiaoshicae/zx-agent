//! The JSON body of a Messages request, and the conversion options recorded with each request
//! (pi: anthropic-messages.ts).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use zx_ai::{
    AssistantMessage, ContentBlock, EMPTY_TOOL_RESULT, Image, MISSING_TOOL_RESULT, Message, Reasoning, Request,
    SystemPrompt, TOOL_IMAGES_OMITTED, ToolDef, ToolResultMessage, USER_IMAGES_OMITTED, api_identifier, with_note,
};

use crate::config::{BodyOptions, REASONING_FIELDS, SystemUpdates};

/// The name of this conversion in the recorded options.
const FORMAT: &str = "anthropic-messages";
/// The version of this conversion in the recorded options, the only one replayed.
const VERSION: u32 = 2;

/// The output limit of a request that sets none, which the API requires: room for long
/// answers on current models, whose responses stream.
pub(crate) const DEFAULT_MAX_TOKENS: u32 = 64_000;

/// What a thinking block the API redacted is recorded with in place of its signature: this
/// marker, then the block's encrypted data. No signature starts with it.
pub(crate) const REDACTED: &str = "redacted:";

/// What a failed tool result says in place of its images, which the API refuses in a failed
/// result: they follow the results of the turn instead (pi: `ERROR_TOOL_IMAGE_PLACEHOLDER`).
const IMAGES_AFTER_RESULTS: &str = "(image attached after the tool results)";

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

/// The conversion a record names. Read apart from the options, which refuse a field they do
/// not know: serde cannot do that for a flattened value.
#[derive(Deserialize)]
struct Conversion {
    format: String,
    version: u32,
}

/// Rebuild the JSON body of a Messages request from a recorded request and the options
/// recorded with it, for the request audit; as zx-ai-openai's `replay_body` does for its
/// conversion. Credentials, URLs and HTTP headers are not in it. Rejects another conversion,
/// any version but this one and an option it does not know, rather than apply a different
/// conversion.
pub fn replay_body(request: &Request, provider: &str, options: &Value) -> anyhow::Result<Value> {
    let Conversion { format, version } = Conversion::deserialize(options)?;
    anyhow::ensure!(
        format == FORMAT,
        "request conversion format {format:?} is not supported; only {FORMAT:?} can be replayed"
    );
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

/// The JSON body of a streaming Messages request to `provider`, the name its answers are
/// recorded under. The system prompt, the tools and the end of the conversation are cache
/// breakpoints, so that a conversation's next request reads what this one wrote.
pub(crate) fn build_body(request: &Request, options: &BodyOptions, provider: &str) -> Value {
    let (system, messages) = convert_messages(request, options.system_updates, provider);
    let mut body = json!({
        "model": request.model,
        "max_tokens": request.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        "messages": messages,
        "stream": true,
    });
    if let Some(system) = system {
        body["system"] = json!([{ "type": "text", "text": system, "cache_control": cache_breakpoint() }]);
    }
    if !request.tools.is_empty() {
        body["tools"] = tools(&request.tools);
    }
    // Checked when the provider is built: none replaces a field set above.
    for (field, value) in &options.extra_fields {
        body[field.as_str()] = value.clone();
    }
    // After the extra fields, which a request without a level sends as they are.
    if let Some(reasoning) = request.reasoning {
        apply_reasoning(&mut body, reasoning);
    }
    body
}

/// The cache breakpoint of this provider: the API's default lifetime.
fn cache_breakpoint() -> Value {
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
                definition["cache_control"] = cache_breakpoint();
            }
            definition
        })
        .collect()
}

/// Write `reasoning` into `body`: adaptive thinking at an effort, with the thinking shown as a
/// summary, or thinking turned off, which says neither how it is shown nor an effort. Minimal
/// is the API's lowest effort, `low`. The level alone decides [`REASONING_FIELDS`], so what
/// the extra fields put there goes first ([`BodyOptions::extra_fields`]); the other settings
/// of a `thinking` or an `output_config` among them stay, such as `thinking.block_binding`.
fn apply_reasoning(body: &mut Value, reasoning: Reasoning) {
    for (object, key) in REASONING_FIELDS {
        let emptied = body
            .get_mut(object)
            .and_then(Value::as_object_mut)
            .is_some_and(|settings| settings.remove(key).is_some() && settings.is_empty());
        // An object that held nothing but the level's settings is not sent empty.
        if emptied && let Some(sent) = body.as_object_mut() {
            sent.remove(object);
        }
    }
    let effort = match reasoning {
        Reasoning::Off => {
            object(body, "thinking").insert("type".into(), json!("disabled"));
            return;
        }
        Reasoning::Minimal | Reasoning::Low => "low",
        Reasoning::Medium => "medium",
        Reasoning::High => "high",
        Reasoning::Xhigh => "xhigh",
    };
    let thinking = object(body, "thinking");
    thinking.insert("type".into(), json!("adaptive"));
    thinking.insert("display".into(), json!("summarized"));
    object(body, "output_config").insert("effort".into(), json!(effort));
}

/// The object `body[field]`, an empty one in place of anything else there.
fn object<'a>(body: &'a mut Value, field: &str) -> &'a mut Map<String, Value> {
    if !body[field].is_object() {
        body[field] = json!({});
    }
    body[field].as_object_mut().expect("the field holds an object now")
}

/// The conversation's turns as the body's `messages` are built: a block joins the turn before
/// it when the role is the same, since the API takes alternating turns.
#[derive(Default)]
struct Turns {
    turns: Vec<Value>,
    /// The images of failed results, each run after a line naming its call, for after the
    /// results of the turn: the API takes text alone in a failed result.
    deferred: Vec<Value>,
}

impl Turns {
    fn push(&mut self, role: &str, block: Value) {
        if role != "system"
            && let Some(last) = self.turns.last_mut()
            && last["role"] == role
            && let Some(content) = last["content"].as_array_mut()
        {
            content.push(block);
            return;
        }
        self.turns.push(json!({ "role": role, "content": [block] }));
    }

    /// End a run of tool results: a result for each call of `pending` that has none, then the
    /// images of the failed results, which the API takes after all results of the turn.
    fn close_calls(&mut self, pending: &mut Vec<String>) {
        for id in pending.drain(..) {
            self.push("user", tool_result(&id, MISSING_TOOL_RESULT, true));
        }
        for block in std::mem::take(&mut self.deferred) {
            self.push("user", block);
        }
    }

    /// The result of a tool call with its images, or, for a failed result, with a note
    /// that they follow the results of the turn; to a model that takes no images, with a
    /// note in their place.
    fn push_result(&mut self, result: &ToolResultMessage, takes_images: bool) {
        let id = result.tool_call_id.as_str();
        if result.images.is_empty() {
            let text = if result.content.is_empty() {
                EMPTY_TOOL_RESULT
            } else {
                result.content.as_str()
            };
            self.push("user", tool_result(id, text, result.is_error));
        } else if !takes_images {
            let text = with_note(&result.content, TOOL_IMAGES_OMITTED);
            self.push("user", tool_result(id, text, result.is_error));
        } else if result.is_error {
            let text = with_note(&result.content, IMAGES_AFTER_RESULTS);
            self.push("user", tool_result(id, text, true));
            let caption = format!("Images from the error result of tool call {id} ({}):", result.tool_name);
            self.deferred.push(text_block(&caption));
            self.deferred.extend(result.images.iter().map(image_block));
        } else {
            let text = (!result.content.is_empty()).then(|| text_block(&result.content));
            let content: Vec<Value> = text.into_iter().chain(result.images.iter().map(image_block)).collect();
            self.push("user", tool_result(id, content, false));
        }
    }

    /// The system prompt changes held back, as one system turn, which the API takes after a
    /// user turn and before an answer.
    fn release(&mut self, held: &mut Vec<String>) {
        if !held.is_empty() {
            self.push("system", text_block(&held.join("\n\n")));
            held.clear();
        }
    }

    /// Make the end of the conversation a cache breakpoint. Only a user or system turn ends
    /// with a block that takes one.
    fn mark_end_cached(&mut self) {
        if let Some(last) = self.turns.last_mut()
            && last["role"] != "assistant"
            && let Some(block) = last["content"].as_array_mut().and_then(|content| content.last_mut())
        {
            block["cache_control"] = cache_breakpoint();
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
/// Thinking is sent back only to the provider and model that produced it: its signature is
/// valid for that model alone (pi: `transformMessages`). The images of a user message precede
/// its text, as the API advises; to a model that takes no images, a note goes in their place.
fn convert_messages(request: &Request, updates: SystemUpdates, provider: &str) -> (Option<String>, Vec<Value>) {
    let (messages, model) = (&request.messages, request.model.as_str());
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
            Message::User { content, images, .. } => {
                turns.close_calls(&mut pending);
                let text = if images.is_empty() || request.takes_images {
                    images.iter().for_each(|image| turns.push("user", image_block(image)));
                    content.clone()
                } else {
                    with_note(content, USER_IMAGES_OMITTED)
                };
                if !text.trim().is_empty() {
                    turns.push("user", text_block(&text));
                }
            }
            Message::ToolResult(result) => {
                let Some(position) = pending.iter().position(|id| *id == result.tool_call_id) else {
                    continue;
                };
                pending.remove(position);
                turns.push_result(result, request.takes_images);
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
    (system, turns.turns)
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
                    blocks.push(text_block(written));
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

fn text_block(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

/// An image as the API takes it, in base64.
fn image_block(image: &Image) -> Value {
    json!({ "type": "image", "source": { "type": "base64", "media_type": image.mime_type, "data": image.data } })
}

/// The result of the call `id`: its `content`, a text or a list of blocks.
fn tool_result(id: &str, content: impl Into<Value>, is_error: bool) -> Value {
    json!({ "type": "tool_result", "tool_use_id": tool_id(id), "content": content.into(), "is_error": is_error })
}

/// A tool call's id in the form the API takes, letters, digits, `_` and `-`, at most 64
/// characters: the ids of another provider's answers may hold other characters (pi:
/// `normalizeToolCallId`).
fn tool_id(id: &str) -> String {
    let mut id = api_identifier(id);
    // ASCII only now, so a byte is a character.
    id.truncate(64);
    id
}

#[cfg(test)]
mod tests;
