use serde_json::Map;
use zx_ai::{PREAMBLE, SectionChange, StopReason, SystemMessage, ToolCall, ToolResultMessage};

use super::*;

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

/// An answer of the provider `p` with `content`.
fn answer(content: Vec<ContentBlock>) -> Message {
    Message::Assistant(AssistantMessage {
        content,
        ..AssistantMessage::failure("p", "m", StopReason::Stop, "")
    })
}

fn call(id: &str) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: "bash".into(),
        arguments: json!({"command": "ls"}),
        arguments_error: None,
    })
}

fn result(id: &str, content: &str) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: id.into(),
        tool_name: "bash".into(),
        content: content.into(),
        is_error: false,
        timestamp: 0,
        duration_ms: None,
        details: None,
        usage: None,
    })
}

fn request(messages: Vec<Message>) -> Request {
    Request {
        model: "m".into(),
        messages,
        tools: vec![],
        max_tokens: None,
        reasoning: None,
        cache_key: None,
    }
}

/// The body of `request` to the provider `p`, written as `updates` says.
fn body_of(request: &Request, updates: SystemUpdates) -> Value {
    let options = BodyOptions {
        system_updates: updates,
        ..BodyOptions::default()
    };
    build_body(request, &options, "p")
}

#[test]
fn a_conversation_becomes_alternating_turns_with_its_end_cached() {
    let body = body_of(
        &request(vec![
            system(&[(PREAMBLE, Some("Be brief."))]),
            Message::user("look"),
            answer(vec![
                ContentBlock::Text {
                    text: "Looking.".into(),
                },
                call("a"),
                call("b"),
            ]),
            result("a", "one"),
            result("b", ""),
            Message::user("and then?"),
        ]),
        SystemUpdates::Collapse,
    );
    assert_eq!(body["max_tokens"], json!(DEFAULT_MAX_TOKENS));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(
        body["system"],
        json!([{"type": "text", "text": "Be brief.", "cache_control": {"type": "ephemeral"}}])
    );
    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": "look"}]},
            {"role": "assistant", "content": [
                {"type": "text", "text": "Looking."},
                {"type": "tool_use", "id": "a", "name": "bash", "input": {"command": "ls"}},
                {"type": "tool_use", "id": "b", "name": "bash", "input": {"command": "ls"}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "a", "content": "one", "is_error": false},
                {"type": "tool_result", "tool_use_id": "b", "content": "(no output)", "is_error": false},
                {"type": "text", "text": "and then?", "cache_control": {"type": "ephemeral"}},
            ]},
        ])
    );
}

#[test]
fn a_call_without_a_result_gets_one_and_a_result_without_its_call_is_left_out() {
    let body = body_of(
        &request(vec![
            Message::user("go"),
            answer(vec![call("a"), call("b")]),
            result("a", "ok"),
            result("z", "stray"),
            Message::user("next"),
        ]),
        SystemUpdates::Collapse,
    );
    let results: Vec<(&str, &str)> = body["messages"][2]["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|block| block["type"] == "tool_result")
        .map(|block| {
            (
                block["tool_use_id"].as_str().unwrap(),
                block["content"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(results, [("a", "ok"), ("b", MISSING_TOOL_RESULT)]);
    assert_eq!(body["messages"][2]["content"][1]["is_error"], json!(true));
}

#[test]
fn thinking_goes_back_with_its_signature_only_to_its_provider_and_model() {
    let thinking = |thinking: &str, signature: Option<&str>| ContentBlock::Thinking {
        thinking: thinking.into(),
        signature: signature.map(str::to_string),
    };
    let mine = answer(vec![
        thinking("reasoned", Some("sig-1")),
        thinking("", Some("redacted:opaque")),
        // Cut off before its signature arrived.
        thinking("half", None),
        ContentBlock::Text { text: "done".into() },
    ]);
    let other_source = |provider: &str, model: &str| {
        Message::Assistant(AssistantMessage {
            content: vec![
                thinking("other", Some("sig-2")),
                ContentBlock::Text { text: "theirs".into() },
            ],
            ..AssistantMessage::failure(provider, model, StopReason::Stop, "")
        })
    };
    let body = body_of(
        &request(vec![
            Message::user("a"),
            other_source("openai", "m"),
            Message::user("b"),
            // The same provider before a switch of model.
            other_source("p", "earlier-model"),
            Message::user("c"),
            mine,
            Message::user("d"),
        ]),
        SystemUpdates::Collapse,
    );
    for theirs in [1, 3] {
        assert_eq!(
            body["messages"][theirs]["content"],
            json!([{"type": "text", "text": "theirs"}])
        );
    }
    assert_eq!(
        body["messages"][5]["content"],
        json!([
            {"type": "thinking", "thinking": "reasoned", "signature": "sig-1"},
            {"type": "redacted_thinking", "data": "opaque"},
            {"type": "text", "text": "done"},
        ])
    );
}

#[test]
fn inline_changes_of_the_system_prompt_wait_for_the_next_answer() {
    let messages = vec![
        system(&[(PREAMBLE, Some("Be brief.")), ("environment", Some("cwd /a"))]),
        Message::user("look"),
        answer(vec![call("a")]),
        // A change between a call and its result.
        system(&[("environment", Some("cwd /b"))]),
        result("a", "ok"),
        answer(vec![ContentBlock::Text { text: "seen".into() }]),
        system(&[("environment", None)]),
        Message::user("again"),
    ];
    let inline = body_of(&request(messages.clone()), SystemUpdates::Inline);
    let roles: Vec<&str> = inline["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|turn| turn["role"].as_str().unwrap())
        .collect();
    assert_eq!(
        roles,
        ["user", "assistant", "user", "system", "assistant", "user", "system"]
    );
    assert!(inline["system"][0]["text"].as_str().unwrap().contains("cwd /a"));
    assert!(
        inline["messages"][3]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("cwd /b")
    );
    // The last turn is the end of the conversation, cached.
    assert_eq!(inline["messages"][6]["content"][0]["cache_control"], cache());

    let collapsed = body_of(&request(messages), SystemUpdates::Collapse);
    let system = collapsed["system"][0]["text"].as_str().unwrap();
    assert!(system.starts_with("Be brief.") && !system.contains("cwd"), "{system}");
    assert!(
        collapsed["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|turn| turn["role"] != "system")
    );
}

#[test]
fn a_reasoning_level_sets_adaptive_thinking_at_an_effort_or_turns_thinking_off() {
    let mut extra_fields = Map::new();
    extra_fields.insert(
        "output_config".into(),
        json!({"task_budget": {"type": "tokens", "total": 20000}}),
    );
    let options = BodyOptions {
        extra_fields,
        ..BodyOptions::default()
    };
    let with = |reasoning| {
        build_body(
            &Request {
                reasoning: Some(reasoning),
                ..request(vec![Message::user("hi")])
            },
            &options,
            "p",
        )
    };
    let high = with(Reasoning::High);
    assert_eq!(high["thinking"], json!({"type": "adaptive", "display": "summarized"}));
    assert_eq!(
        high["output_config"],
        json!({"task_budget": {"type": "tokens", "total": 20000}, "effort": "high"})
    );
    assert_eq!(with(Reasoning::Minimal)["output_config"]["effort"], json!("low"));
    assert_eq!(with(Reasoning::Xhigh)["output_config"]["effort"], json!("xhigh"));
    let off = with(Reasoning::Off);
    assert_eq!(off["thinking"], json!({"type": "disabled"}));
    assert!(off["output_config"].get("effort").is_none());
    // Without a level, nothing is said: the model's default.
    let unset = body_of(&request(vec![Message::user("hi")]), SystemUpdates::Collapse);
    assert!(unset.get("thinking").is_none() && unset.get("output_config").is_none());
}

#[test]
fn tools_are_declared_with_the_last_one_cached_and_ids_take_the_apis_form() {
    let tool = |name: &str| ToolDef {
        name: name.into(),
        description: format!("{name} things"),
        parameters: json!({"type": "object", "properties": {}}),
    };
    let body = build_body(
        &Request {
            tools: vec![tool("read"), tool("write")],
            max_tokens: Some(500),
            ..request(vec![
                Message::user("go"),
                answer(vec![call("call:1/x")]),
                result("call:1/x", "ok"),
            ])
        },
        &BodyOptions::default(),
        "p",
    );
    assert_eq!(body["max_tokens"], json!(500));
    assert_eq!(
        body["tools"],
        json!([
            {"name": "read", "description": "read things", "input_schema": {"type": "object", "properties": {}}},
            {"name": "write", "description": "write things", "input_schema": {"type": "object", "properties": {}},
             "cache_control": {"type": "ephemeral"}},
        ])
    );
    assert_eq!(body["messages"][1]["content"][0]["id"], json!("call_1_x"));
    assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], json!("call_1_x"));
}

#[test]
fn a_reasoning_level_keeps_the_other_thinking_settings_of_the_extra_fields() {
    let binding = json!({"prefix_mismatch_behavior": "drop_block"});
    let mut extra_fields = Map::new();
    extra_fields.insert(
        "thinking".into(),
        json!({"block_binding": binding, "display": "omitted"}),
    );
    let options = BodyOptions {
        extra_fields,
        ..BodyOptions::default()
    };
    let with = |reasoning| {
        build_body(
            &Request {
                reasoning: Some(reasoning),
                ..request(vec![Message::user("hi")])
            },
            &options,
            "p",
        )
    };
    assert_eq!(
        with(Reasoning::Medium)["thinking"],
        json!({"block_binding": binding, "type": "adaptive", "display": "summarized"})
    );
    // The level says how thinking is shown too, and thinking that is off shows nothing.
    assert_eq!(
        with(Reasoning::Off)["thinking"],
        json!({"block_binding": binding, "type": "disabled"})
    );
}

#[test]
fn the_fields_a_reasoning_level_sets_are_those_extra_fields_must_not_set() {
    for reasoning in Reasoning::ALL {
        let mut body = json!({});
        apply_reasoning(&mut body, reasoning);
        for (object, fields) in body.as_object().unwrap() {
            for key in fields.as_object().unwrap().keys() {
                assert!(
                    crate::config::REASONING_FIELDS
                        .iter()
                        .any(|&(reserved, field)| reserved == object && field == key),
                    "{reasoning}: {object}.{key}"
                );
            }
        }
    }
}

#[test]
fn replayed_bodies_match_the_live_body_in_both_system_update_modes() {
    let request = request(vec![
        system(&[("preamble", Some("Be brief."))]),
        Message::user("first"),
        system(&[("environment", Some("changed"))]),
        Message::user("second"),
    ]);
    let mut extra_fields = Map::new();
    extra_fields.insert("metadata".into(), json!({"user_id": "u"}));
    for system_updates in [SystemUpdates::Collapse, SystemUpdates::Inline] {
        let options = BodyOptions {
            system_updates,
            extra_fields: extra_fields.clone(),
        };
        let replayed = replay_body(&request, "p", &request_options(&options)).unwrap();
        assert_eq!(replayed, build_body(&request, &options, "p"), "{system_updates:?}");
    }
}

#[test]
fn replay_refuses_another_conversion_and_options_it_does_not_know() {
    let request = request(vec![Message::user("hi")]);
    for options in [
        Value::Null,
        json!({"format": "openai-chat-completions", "version": 1, "systemUpdates": "collapse"}),
        json!({"format": "anthropic-messages", "version": 2, "systemUpdates": "collapse"}),
        json!({"format": "anthropic-messages", "version": 1, "systemUpdates": "sideways"}),
        json!({"format": "anthropic-messages", "version": 1, "systemUpdates": "collapse", "extra": 1}),
        json!({"format": "anthropic-messages", "version": 1}),
        json!({"format": "anthropic-messages", "version": 1, "systemUpdates": "collapse", "extraFields": {"model": "x"}}),
    ] {
        assert!(replay_body(&request, "p", &options).is_err(), "{options}");
    }
}

#[test]
fn the_record_names_the_conversion_and_its_options() {
    let options = BodyOptions {
        system_updates: SystemUpdates::Inline,
        ..BodyOptions::default()
    };
    assert_eq!(
        request_options(&options),
        json!({"format": "anthropic-messages", "version": 1, "systemUpdates": "inline"})
    );
}
