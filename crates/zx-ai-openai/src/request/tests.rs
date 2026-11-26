use serde_json::Map;
use zx_ai::Provider;
use zx_ai::{ApiKey, SectionChange, StopReason, SystemMessage, ToolCall, ToolDef, ToolResultMessage, Usage};

use super::*;
use crate::OpenAiCompatible;
use crate::config::{MaxTokensField, OpenAiConfig, RESERVED_FIELDS};

/// The options of a provider that sends system prompt changes as `system_updates` says and
/// leaves every other setting at its default.
fn options(system_updates: SystemUpdates) -> BodyOptions {
    BodyOptions {
        system_updates,
        ..BodyOptions::default()
    }
}

/// The body of `request` to the provider `p`, written as `options` say.
fn body_of(request: &Request, options: &BodyOptions) -> Value {
    build_body(request, options, "p")
}

/// A provider named `p` that writes bodies as `body` says. Its address and key are not part
/// of what a request records.
fn provider(body: BodyOptions) -> OpenAiCompatible {
    OpenAiCompatible::new(OpenAiConfig {
        base_url: "https://example.invalid/v1".into(),
        api_key: ApiKey::fixed("sk-not-recorded"),
        provider_name: "p".into(),
        body,
        ..OpenAiConfig::default()
    })
    .unwrap()
}

#[test]
fn body_pairs_tool_calls_with_results() {
    let assistant = AssistantMessage::new(
        "p",
        "m",
        StopReason::ToolUse,
        vec![
            ContentBlock::ToolCall(ToolCall {
                id: "a".into(),
                name: "bash".into(),
                arguments: json!({}),
                arguments_error: None,
            }),
            ContentBlock::ToolCall(ToolCall {
                id: "b".into(),
                name: "bash".into(),
                arguments: json!({}),
                arguments_error: None,
            }),
        ],
    );
    let result = |id: &str| {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: id.into(),
            tool_name: "bash".into(),
            content: "ok".into(),
            images: Vec::new(),
            is_error: false,
            timestamp: 0,
            duration_ms: None,
            details: None,
            usage: None,
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
        takes_images: false,
    };
    let body = body_of(&request, &options(SystemUpdates::default()));
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
    let assistant = AssistantMessage::new(
        "p",
        "m",
        StopReason::ToolUse,
        vec![ContentBlock::ToolCall(ToolCall {
            id: "a".into(),
            name: "bash".into(),
            arguments: json!({}),
            arguments_error: None,
        })],
    );
    let request = Request {
        model: "m".into(),
        messages: vec![
            Message::user("go"),
            Message::Assistant(assistant),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "a".into(),
                tool_name: "bash".into(),
                content: "ok".into(),
                images: Vec::new(),
                is_error: false,
                timestamp: 0,
                duration_ms: None,
                details: Some(json!({"onlyForObservers": true})),
                usage: Some(Usage {
                    input: 1,
                    output: 1,
                    cache_read: 0,
                    cache_write: 0,
                    reasoning: 0,
                    total_tokens: 2,
                    total_cost: None,
                }),
            }),
        ],
        tools: vec![],
        max_tokens: None,
        reasoning: None,
        cache_key: None,
        takes_images: false,
    };
    let body = body_of(&request, &options(SystemUpdates::default()));
    assert_eq!(
        body["messages"][2],
        json!({"role": "tool", "tool_call_id": "a", "content": "ok"})
    );
    assert!(!body.to_string().contains("onlyForObservers"));
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
        takes_images: false,
    };
    let body = body_of(&request, &options(system_updates));
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
    Message::Assistant(AssistantMessage::new("p", model, StopReason::Stop, content))
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
        takes_images: false,
    };
    let body = body_of(&request, &options(SystemUpdates::default()));
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
        takes_images: false,
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
    let filling = BodyOptions {
        fill_missing_reasoning: true,
        ..BodyOptions::default()
    };
    let body = body_of(&partly_reasoned(), &filling);
    assert_eq!(second_answer(&body)["reasoning_content"], "");
}

#[test]
fn leaves_missing_reasoning_alone_unless_asked() {
    let body = body_of(&partly_reasoned(), &BodyOptions::default());
    assert!(second_answer(&body).get("reasoning_content").is_none(), "{body}");
}

#[test]
fn replay_leaves_missing_reasoning_alone_unless_the_options_ask() {
    let request = partly_reasoned();
    let options = json!({"format": "openai-chat-completions", "version": 3, "systemUpdates": "collapse"});
    let body = replay_body(&request, "p", &options).unwrap();
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
        takes_images: false,
    }
}

#[test]
fn replayed_bodies_match_the_live_body_in_both_system_update_modes() {
    let request = replayable_request();
    for system_updates in [SystemUpdates::Collapse, SystemUpdates::Inline] {
        let live = options(system_updates);
        let provider = provider(live.clone());
        let replayed = replay_body(&request, provider.name(), &provider.request_options()).unwrap();
        assert_eq!(replayed, body_of(&request, &live), "{system_updates:?}");
    }
}

#[test]
fn request_options_hold_only_the_conversion_policy() {
    assert_eq!(
        provider(options(SystemUpdates::Inline)).request_options(),
        json!({"format": "openai-chat-completions", "version": 3, "systemUpdates": "inline"})
    );
    let filling = BodyOptions {
        fill_missing_reasoning: true,
        ..BodyOptions::default()
    };
    let recorded = provider(filling.clone()).request_options();
    assert_eq!(recorded["fillMissingReasoning"], json!(true));
    let request = partly_reasoned();
    assert_eq!(
        replay_body(&request, "p", &recorded).unwrap(),
        body_of(&request, &filling)
    );
}

#[test]
fn every_option_is_recorded_under_its_name_and_replays() {
    let all = BodyOptions {
        system_updates: SystemUpdates::Inline,
        extra_fields: fields(&[("seed", json!(7))]),
        fill_missing_reasoning: true,
        reasoning_format: ReasoningFormat::ReasoningObject,
        prompt_cache_key: true,
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
    };
    let recorded = provider(all.clone()).request_options();
    assert_eq!(
        recorded,
        json!({
            "format": "openai-chat-completions",
            "version": 3,
            "systemUpdates": "inline",
            "extraFields": {"seed": 7},
            "fillMissingReasoning": true,
            "reasoningFormat": "reasoning-object",
            "promptCacheKey": true,
            "maxTokensField": "max_completion_tokens",
        })
    );
    let request = Request {
        reasoning: Some(Reasoning::Low),
        cache_key: Some("session-1".into()),
        ..replayable_request()
    };
    assert_eq!(replay_body(&request, "p", &recorded).unwrap(), body_of(&request, &all));
}

#[test]
fn replay_rejects_unknown_conversion_options() {
    let request = replayable_request();
    for options in [
        Value::Null,
        json!({"format": "other", "version": 3, "systemUpdates": "collapse"}),
        json!({"format": "openai-chat-completions", "version": 4, "systemUpdates": "collapse"}),
        // An older version is not replayed either.
        json!({"format": "openai-chat-completions", "version": 2, "systemUpdates": "collapse"}),
        json!({"format": "openai-chat-completions", "version": 3, "systemUpdates": "sideways"}),
        json!({"format": "openai-chat-completions", "version": 3, "systemUpdates": "collapse", "extra": 1}),
        json!({"format": "openai-chat-completions", "version": 3}),
        json!({"format": "openai-chat-completions", "version": 3, "systemUpdates": "collapse", "maxTokensField": "max_output_tokens"}),
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
    let with_fields = BodyOptions {
        extra_fields: fields(&[
            ("thinking", json!({"type": "disabled"})),
            ("reasoning_effort", json!("low")),
        ]),
        ..BodyOptions::default()
    };
    let request = replayable_request();
    let body = body_of(&request, &with_fields);
    assert_eq!(body["thinking"], json!({"type": "disabled"}));
    assert_eq!(body["reasoning_effort"], json!("low"));
    let recorded = provider(with_fields).request_options();
    assert_eq!(
        recorded["extraFields"],
        json!({"thinking": {"type": "disabled"}, "reasoning_effort": "low"})
    );
    // The audit rebuilds the body that was sent.
    assert_eq!(replay_body(&request, "p", &recorded).unwrap(), body);
}

#[test]
fn fields_the_provider_sets_cannot_be_added() {
    for field in RESERVED_FIELDS {
        let extra_fields = fields(&[(field, json!(false))]);
        let error = OpenAiCompatible::new(OpenAiConfig {
            body: BodyOptions {
                extra_fields: extra_fields.clone(),
                ..BodyOptions::default()
            },
            ..OpenAiConfig::default()
        })
        .err()
        .unwrap();
        assert!(error.to_string().contains(field), "{error}");
        let options = json!({
            "format": "openai-chat-completions",
            "version": 3,
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
    let options = BodyOptions {
        reasoning_format: format,
        ..BodyOptions::default()
    };
    body_of(&request, &options)
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
    let fixed = BodyOptions {
        extra_fields: fields(&[("reasoning_effort", json!("low"))]),
        ..BodyOptions::default()
    };
    let request = Request {
        reasoning: Some(Reasoning::High),
        ..replayable_request()
    };
    assert_eq!(body_of(&request, &fixed)["reasoning_effort"], "high");
    assert_eq!(body_of(&replayable_request(), &fixed)["reasoning_effort"], "low");
}

#[test]
fn a_request_set_to_off_sends_no_fixed_value_of_the_fields_its_format_writes() {
    let off = Request {
        reasoning: Some(Reasoning::Off),
        ..replayable_request()
    };
    let cases = [
        (
            ReasoningFormat::Effort,
            fields(&[("reasoning_effort", json!("low"))]),
            json!({}),
        ),
        (
            ReasoningFormat::Thinking,
            fields(&[
                ("thinking", json!({"type": "enabled"})),
                ("reasoning_effort", json!("low")),
            ]),
            json!({"thinking": {"type": "disabled"}}),
        ),
        (
            ReasoningFormat::ReasoningObject,
            fields(&[("reasoning", json!({"effort": "high"}))]),
            json!({"reasoning": {"effort": "none"}}),
        ),
    ];
    for (reasoning_format, extra_fields, expected) in cases {
        let fixed = BodyOptions {
            reasoning_format,
            extra_fields,
            ..BodyOptions::default()
        };
        assert_eq!(
            reasoning_fields(&body_of(&off, &fixed)),
            expected,
            "{reasoning_format:?}"
        );
    }
}

#[test]
fn a_reasoning_level_leaves_the_extra_fields_its_format_does_not_write() {
    let fixed = BodyOptions {
        extra_fields: fields(&[("thinking", json!({"type": "enabled"})), ("seed", json!(7))]),
        ..BodyOptions::default()
    };
    let off = Request {
        reasoning: Some(Reasoning::Off),
        ..replayable_request()
    };
    let body = body_of(&off, &fixed);
    assert_eq!(reasoning_fields(&body), json!({"thinking": {"type": "enabled"}}));
    assert_eq!(body["seed"], json!(7));
}

#[test]
fn the_reasoning_format_is_recorded_unless_it_is_the_default_and_replays() {
    let default = provider(BodyOptions::default());
    assert!(default.request_options().get("reasoningFormat").is_none());
    let thinking = BodyOptions {
        reasoning_format: ReasoningFormat::Thinking,
        ..BodyOptions::default()
    };
    let request = Request {
        reasoning: Some(Reasoning::Medium),
        ..replayable_request()
    };
    let body = body_of(&request, &thinking);
    let recorded = provider(thinking).request_options();
    assert_eq!(recorded["reasoningFormat"], "thinking");
    assert_eq!(replay_body(&request, "p", &recorded).unwrap(), body);
    let mut unknown = recorded.clone();
    unknown["reasoningFormat"] = json!("guess");
    assert!(replay_body(&request, "p", &unknown).is_err());
}

#[test]
fn the_cache_key_is_sent_only_when_enabled_and_replays() {
    let request = Request {
        cache_key: Some("session-1".into()),
        ..replayable_request()
    };
    let off = body_of(&request, &BodyOptions::default());
    assert!(off.get("prompt_cache_key").is_none());
    let keyed = BodyOptions {
        prompt_cache_key: true,
        ..BodyOptions::default()
    };
    let on = body_of(&request, &keyed);
    assert_eq!(on["prompt_cache_key"], "session-1");
    let provider = provider(keyed.clone());
    assert_eq!(provider.request_options()["promptCacheKey"], true);
    assert_eq!(
        replay_body(&request, provider.name(), &provider.request_options()).unwrap(),
        on
    );
    // Without a key there is nothing to send, whatever the setting.
    let keyless = body_of(&replayable_request(), &keyed);
    assert!(keyless.get("prompt_cache_key").is_none());
}

#[test]
fn the_output_limit_is_written_in_the_chosen_field_and_replays() {
    let request = replayable_request();
    let default = body_of(&request, &BodyOptions::default());
    assert_eq!(default["max_tokens"], 64);
    assert!(default.get("max_completion_tokens").is_none(), "{default}");
    assert!(
        provider(BodyOptions::default())
            .request_options()
            .get("maxTokensField")
            .is_none()
    );

    let newer = BodyOptions {
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
        ..BodyOptions::default()
    };
    let body = body_of(&request, &newer);
    assert_eq!(body["max_completion_tokens"], 64);
    assert!(body.get("max_tokens").is_none(), "{body}");
    let recorded = provider(newer.clone()).request_options();
    assert_eq!(recorded["maxTokensField"], "max_completion_tokens");
    assert_eq!(replay_body(&request, "p", &recorded).unwrap(), body);

    // Without a limit neither field is sent, whichever is chosen.
    let unlimited = Request {
        max_tokens: None,
        ..replayable_request()
    };
    let body = body_of(&unlimited, &newer);
    assert!(
        body.get("max_tokens").is_none() && body.get("max_completion_tokens").is_none(),
        "{body}"
    );
}

/// An image with a stand-in for its data, which the conversion does not read.
fn png() -> Image {
    Image {
        mime_type: "image/png".into(),
        data: "AAAA".into(),
    }
}

/// The content part of [`png`].
fn png_part() -> Value {
    json!({"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}})
}

/// A request of `messages` to a model that takes images when `takes_images` says so.
fn image_request(messages: Vec<Message>, takes_images: bool) -> Request {
    Request {
        model: "m".into(),
        messages,
        tools: Vec::new(),
        max_tokens: None,
        reasoning: None,
        cache_key: None,
        takes_images,
    }
}

/// An answer of `p` that calls `read` once for each of `ids`.
fn reads(ids: &[&str]) -> Message {
    let calls = ids
        .iter()
        .map(|id| {
            ContentBlock::ToolCall(ToolCall {
                id: (*id).into(),
                name: "read".into(),
                arguments: json!({}),
                arguments_error: None,
            })
        })
        .collect();
    Message::Assistant(AssistantMessage::new("p", "m", StopReason::ToolUse, calls))
}

/// The result of the call `id`: `content` and an image.
fn image_result(id: &str, content: &str) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: id.into(),
        tool_name: "read".into(),
        content: content.into(),
        images: vec![png()],
        is_error: false,
        timestamp: 0,
        duration_ms: None,
        details: None,
        usage: None,
    })
}

#[test]
fn a_user_message_sends_its_images_before_its_text() {
    let request = image_request(vec![Message::user_with_images("what is this?", vec![png()])], true);
    let body = body_of(&request, &BodyOptions::default());
    assert_eq!(
        body["messages"][0],
        json!({"role": "user", "content": [png_part(), {"type": "text", "text": "what is this?"}]})
    );
}

#[test]
fn the_images_of_tool_results_follow_the_results_in_a_user_message() {
    let request = image_request(
        vec![
            Message::user("look"),
            reads(&["a", "b"]),
            image_result("a", "Read image file [image/png]"),
            image_result("b", ""),
        ],
        true,
    );
    let body = body_of(&request, &BodyOptions::default());
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 5, "{body}");
    assert_eq!(
        messages[2],
        json!({"role": "tool", "tool_call_id": "a", "content": "Read image file [image/png]"})
    );
    assert_eq!(
        messages[3],
        json!({"role": "tool", "tool_call_id": "b", "content": "(see attached image)"})
    );
    assert_eq!(
        messages[4],
        json!({"role": "user", "content": [
            {"type": "text", "text": "Attached image(s) from tool result:"}, png_part(), png_part()
        ]})
    );
}

#[test]
fn a_model_that_takes_no_images_gets_a_note_in_their_place() {
    let request = image_request(
        vec![
            Message::user_with_images("what is this?", vec![png()]),
            reads(&["a"]),
            image_result("a", "Read image file [image/png]"),
        ],
        false,
    );
    let body = body_of(&request, &BodyOptions::default());
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3, "{body}");
    assert_eq!(
        messages[0]["content"],
        "what is this?\n(image omitted: model does not support images)"
    );
    assert_eq!(
        messages[2]["content"],
        "Read image file [image/png]\n(tool image omitted: model does not support images)"
    );
}
