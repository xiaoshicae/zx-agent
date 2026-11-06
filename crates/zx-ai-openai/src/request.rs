//! The JSON body of a Chat Completions request, the conversion options recorded with each
//! request, and the body rebuilt from them for the request audit (pi: transform-messages.ts).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use zx_ai::{AssistantMessage, ContentBlock, Message, Reasoning, Request, SystemPrompt};

use crate::accumulate::REASONING_FIELDS;
use crate::config::{OpenAiConfig, ReasoningFormat, SystemUpdates, check_extra_fields};

/// The name of this conversion in the recorded options.
const FORMAT: &str = "openai-chat-completions";

/// The conversion options recorded with each request: the policy that turns a transcript
/// into a body, and nothing secret. Fields absent unless set stay out of the record, so that
/// options recorded before they existed still match.
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
        // Version 1 filled missing reasoning in every request; version 2 only when the
        // options say so (`replay_body` keeps both).
        version: 2,
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
/// Credentials, URLs and HTTP headers are intentionally absent. Rejects unknown option
/// formats/versions rather than silently applying a different conversion policy.
pub fn replay_body(request: &Request, provider: &str, options: &Value) -> anyhow::Result<Value> {
    let options: Options = serde_json::from_value(options.clone())?;
    anyhow::ensure!(options.format == FORMAT, "unsupported request conversion format");
    let fill_missing_reasoning = match (options.version, options.fill_missing_reasoning) {
        // Requests recorded before the fill became an option always had it.
        (1, None) => true,
        (2, fill) => fill.unwrap_or(false),
        _ => anyhow::bail!("unsupported request conversion version"),
    };
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
mod tests {
    use zx_ai::provider::Provider;
    use zx_ai::{SectionChange, StopReason, SystemMessage, ToolDef, ToolResultMessage};

    use super::*;
    use crate::OpenAiCompatible;
    use crate::config::{ApiKey, RESERVED_FIELDS};

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
                details: None,
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
            reasoning: None,
            cache_key: None,
        };
        let body = build_body(&request, &p_config(SystemUpdates::default()));
        let roles: Vec<&str> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|message| message["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["system", "user", "assistant", "tool", "tool", "user"]);
        assert_eq!(body["messages"][4]["tool_call_id"], "b");
        assert_eq!(body["messages"][4]["content"], "No result provided");
        assert_eq!(body["max_tokens"], 100);
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn tool_result_details_stay_out_of_the_body() {
        let assistant = AssistantMessage {
            content: vec![ContentBlock::ToolCall {
                id: "a".into(),
                name: "bash".into(),
                arguments: json!({}),
                arguments_error: None,
            }],
            ..AssistantMessage::failure("p", "m", StopReason::ToolUse, "")
        };
        let request = Request {
            model: "m".into(),
            messages: vec![
                Message::user("go"),
                Message::Assistant(assistant),
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: "a".into(),
                    tool_name: "bash".into(),
                    content: "ok".into(),
                    is_error: false,
                    timestamp: 0,
                    duration_ms: None,
                    details: Some(json!({"onlyForObservers": true})),
                }),
            ],
            tools: vec![],
            max_tokens: None,
            reasoning: None,
            cache_key: None,
        };
        let body = build_body(&request, &p_config(SystemUpdates::default()));
        assert_eq!(
            body["messages"][2],
            json!({"role": "tool", "tool_call_id": "a", "content": "ok"})
        );
        assert!(!body.to_string().contains("onlyForObservers"));
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
            reasoning: None,
            cache_key: None,
        };
        let body = build_body(&request, &p_config(system_updates));
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|message| {
                let content = message["content"].as_str().unwrap_or_default();
                (message["role"].as_str().unwrap().to_string(), content.to_string())
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
            reasoning: None,
            cache_key: None,
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

    /// A history in which only the first answer carries reasoning.
    fn partly_reasoned() -> Request {
        Request {
            model: "m".into(),
            messages: vec![
                Message::user("a"),
                reply("m", vec![thinking("plan"), text("one")]),
                Message::user("b"),
                reply("m", vec![text("two")]),
            ],
            tools: vec![],
            max_tokens: None,
            reasoning: None,
            cache_key: None,
        }
    }

    fn second_answer(body: &Value) -> &Value {
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "assistant")
            .nth(1)
            .unwrap()
    }

    #[test]
    fn fills_empty_reasoning_once_the_history_carries_some_when_asked() {
        // A thinking-mode server rejects a history in which a response without reasoning
        // lacks the field.
        let config = OpenAiConfig {
            fill_missing_reasoning: true,
            ..p_config(SystemUpdates::default())
        };
        let body = build_body(&partly_reasoned(), &config);
        assert_eq!(second_answer(&body)["reasoning_content"], "");
    }

    #[test]
    fn leaves_missing_reasoning_alone_unless_asked() {
        let body = build_body(&partly_reasoned(), &p_config(SystemUpdates::default()));
        assert!(second_answer(&body).get("reasoning_content").is_none(), "{body}");
    }

    #[test]
    fn requests_recorded_by_version_one_replay_with_the_reasoning_filled() {
        let request = partly_reasoned();
        let version_one = json!({"format": "openai-chat-completions", "version": 1, "systemUpdates": "collapse"});
        let body = replay_body(&request, "p", &version_one).unwrap();
        assert_eq!(second_answer(&body)["reasoning_content"], "");
        let version_two = json!({"format": "openai-chat-completions", "version": 2, "systemUpdates": "collapse"});
        let body = replay_body(&request, "p", &version_two).unwrap();
        assert!(second_answer(&body).get("reasoning_content").is_none(), "{body}");
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
            reasoning: None,
            cache_key: None,
        }
    }

    #[test]
    fn replayed_bodies_match_the_live_body_in_both_system_update_modes() {
        let request = replayable_request();
        for system_updates in [SystemUpdates::Collapse, SystemUpdates::Inline] {
            let live = OpenAiConfig {
                base_url: "https://example.invalid/v1".into(),
                api_key: ApiKey::fixed("sk-not-recorded"),
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
            api_key: ApiKey::fixed("sk-not-recorded"),
            ..p_config(SystemUpdates::Inline)
        })
        .unwrap();
        assert_eq!(
            provider.request_options(),
            json!({"format": "openai-chat-completions", "version": 2, "systemUpdates": "inline"})
        );
        let filling = OpenAiCompatible::new(OpenAiConfig {
            fill_missing_reasoning: true,
            ..p_config(SystemUpdates::Collapse)
        })
        .unwrap();
        let options = filling.request_options();
        assert_eq!(options["fillMissingReasoning"], json!(true));
        let request = partly_reasoned();
        assert_eq!(
            replay_body(&request, "p", &options).unwrap(),
            build_body(
                &request,
                &OpenAiConfig {
                    fill_missing_reasoning: true,
                    ..p_config(SystemUpdates::Collapse)
                }
            )
        );
    }

    #[test]
    fn replay_rejects_unknown_conversion_options() {
        let request = replayable_request();
        for options in [
            Value::Null,
            json!({"format": "other", "version": 1, "systemUpdates": "collapse"}),
            json!({"format": "openai-chat-completions", "version": 3, "systemUpdates": "collapse"}),
            json!({"format": "openai-chat-completions", "version": 1, "systemUpdates": "sideways"}),
            json!({"format": "openai-chat-completions", "version": 1, "systemUpdates": "collapse", "extra": 1}),
            // Version 1 always filled, so it cannot have recorded a choice.
            json!({"format": "openai-chat-completions", "version": 1, "systemUpdates": "collapse", "fillMissingReasoning": false}),
        ] {
            assert!(replay_body(&request, "p", &options).is_err(), "{options}");
        }
    }

    fn fields(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(field, value)| (field.to_string(), value.clone()))
            .collect()
    }

    #[test]
    fn extra_fields_go_into_the_body_and_the_recorded_options() {
        let config = OpenAiConfig {
            extra_fields: fields(&[
                ("thinking", json!({"type": "disabled"})),
                ("reasoning_effort", json!("low")),
            ]),
            ..p_config(SystemUpdates::Collapse)
        };
        let request = replayable_request();
        let body = build_body(&request, &config);
        assert_eq!(body["thinking"], json!({"type": "disabled"}));
        assert_eq!(body["reasoning_effort"], json!("low"));
        let provider = OpenAiCompatible::new(config).unwrap();
        let options = provider.request_options();
        assert_eq!(
            options["extraFields"],
            json!({"thinking": {"type": "disabled"}, "reasoning_effort": "low"})
        );
        // The audit rebuilds the body that was sent.
        assert_eq!(replay_body(&request, "p", &options).unwrap(), body);
    }

    #[test]
    fn fields_the_provider_sets_cannot_be_added() {
        for field in RESERVED_FIELDS {
            let extra_fields = fields(&[(field, json!(false))]);
            let error = OpenAiCompatible::new(OpenAiConfig {
                extra_fields: extra_fields.clone(),
                ..p_config(SystemUpdates::Collapse)
            })
            .err()
            .unwrap();
            assert!(error.to_string().contains(field), "{error}");
            let options = json!({
                "format": "openai-chat-completions",
                "version": 1,
                "systemUpdates": "collapse",
                "extraFields": extra_fields,
            });
            assert!(replay_body(&replayable_request(), "p", &options).is_err(), "{field}");
        }
    }

    fn reasoned(reasoning: Option<Reasoning>, format: ReasoningFormat) -> Value {
        let request = Request {
            reasoning,
            ..replayable_request()
        };
        let config = OpenAiConfig {
            reasoning_format: format,
            ..p_config(SystemUpdates::Collapse)
        };
        build_body(&request, &config)
    }

    /// The body's reasoning fields, so that assertions do not depend on the rest.
    fn reasoning_fields(body: &Value) -> Value {
        let mut fields = Map::new();
        for field in ["reasoning_effort", "thinking", "reasoning"] {
            if let Some(value) = body.get(field) {
                fields.insert(field.into(), value.clone());
            }
        }
        Value::Object(fields)
    }

    #[test]
    fn each_reasoning_format_writes_its_own_fields() {
        let cases = [
            (
                ReasoningFormat::Effort,
                Reasoning::High,
                json!({"reasoning_effort": "high"}),
            ),
            (ReasoningFormat::Effort, Reasoning::Off, json!({})),
            (
                ReasoningFormat::Thinking,
                Reasoning::Low,
                json!({"thinking": {"type": "enabled"}, "reasoning_effort": "low"}),
            ),
            (
                ReasoningFormat::Thinking,
                Reasoning::Off,
                json!({"thinking": {"type": "disabled"}}),
            ),
            (
                ReasoningFormat::ReasoningObject,
                Reasoning::Xhigh,
                json!({"reasoning": {"effort": "xhigh"}}),
            ),
            (
                ReasoningFormat::ReasoningObject,
                Reasoning::Off,
                json!({"reasoning": {"effort": "none"}}),
            ),
        ];
        for (format, level, expected) in cases {
            assert_eq!(
                reasoning_fields(&reasoned(Some(level), format)),
                expected,
                "{format:?} {level}"
            );
        }
        // Without a level nothing is sent, whatever the format.
        for format in [
            ReasoningFormat::Effort,
            ReasoningFormat::Thinking,
            ReasoningFormat::ReasoningObject,
        ] {
            assert_eq!(reasoning_fields(&reasoned(None, format)), json!({}), "{format:?}");
        }
    }

    #[test]
    fn a_requests_reasoning_level_wins_over_a_fixed_extra_field() {
        let config = OpenAiConfig {
            extra_fields: fields(&[("reasoning_effort", json!("low"))]),
            ..p_config(SystemUpdates::Collapse)
        };
        let request = Request {
            reasoning: Some(Reasoning::High),
            ..replayable_request()
        };
        assert_eq!(build_body(&request, &config)["reasoning_effort"], "high");
        assert_eq!(build_body(&replayable_request(), &config)["reasoning_effort"], "low");
    }

    #[test]
    fn the_reasoning_format_is_recorded_unless_it_is_the_default_and_replays() {
        let default = OpenAiCompatible::new(p_config(SystemUpdates::Collapse)).unwrap();
        assert!(default.request_options().get("reasoningFormat").is_none());
        let config = OpenAiConfig {
            reasoning_format: ReasoningFormat::Thinking,
            ..p_config(SystemUpdates::Collapse)
        };
        let request = Request {
            reasoning: Some(Reasoning::Medium),
            ..replayable_request()
        };
        let body = build_body(&request, &config);
        let options = OpenAiCompatible::new(config).unwrap().request_options();
        assert_eq!(options["reasoningFormat"], "thinking");
        assert_eq!(replay_body(&request, "p", &options).unwrap(), body);
        let mut unknown = options.clone();
        unknown["reasoningFormat"] = json!("guess");
        assert!(replay_body(&request, "p", &unknown).is_err());
    }

    #[test]
    fn the_cache_key_is_sent_only_when_enabled_and_replays() {
        let request = Request {
            cache_key: Some("session-1".into()),
            ..replayable_request()
        };
        let off = build_body(&request, &OpenAiConfig::default());
        assert!(off.get("prompt_cache_key").is_none());
        let config = OpenAiConfig {
            prompt_cache_key: true,
            ..OpenAiConfig::default()
        };
        let on = build_body(&request, &config);
        assert_eq!(on["prompt_cache_key"], "session-1");
        let provider = OpenAiCompatible::new(OpenAiConfig {
            base_url: "https://example.invalid/v1".into(),
            ..config
        })
        .unwrap();
        assert_eq!(provider.request_options()["promptCacheKey"], true);
        assert_eq!(
            replay_body(&request, provider.name(), &provider.request_options()).unwrap(),
            on
        );
        // Without a key there is nothing to send, whatever the setting.
        let keyless = build_body(
            &replayable_request(),
            &OpenAiConfig {
                prompt_cache_key: true,
                ..OpenAiConfig::default()
            },
        );
        assert!(keyless.get("prompt_cache_key").is_none());
    }
}
