use zx_ai::provider::Provider;
use zx_ai::{SectionChange, StopReason, SystemMessage, ToolDef, ToolResultMessage, Usage};

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
                usage: Some(Usage {
                    input: 1,
                    output: 1,
                    cache_read: 0,
                    cache_write: 0,
                    reasoning: 0,
                    total_tokens: 2,
                    cost_usd: None,
                }),
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
