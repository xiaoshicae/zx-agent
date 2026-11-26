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
    Message::Assistant(AssistantMessage::new("p", "m", StopReason::Stop, content))
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
        images: Vec::new(),
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
        takes_images: false,
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

/// The tool results of a body whose answer called `a` and `b`, with a result for `a` and
/// one for a call `z` that the answer did not make.
fn results_of_unpaired_calls() -> Vec<Value> {
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
    body["messages"][2]["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|block| block["type"] == "tool_result")
        .cloned()
        .collect()
}

#[test]
fn a_call_without_a_result_gets_an_error_result() {
    let results = results_of_unpaired_calls();
    assert_eq!(
        (
            &results[1]["tool_use_id"],
            &results[1]["content"],
            &results[1]["is_error"]
        ),
        (&json!("b"), &json!(MISSING_TOOL_RESULT), &json!(true))
    );
}

#[test]
fn a_result_without_its_call_is_left_out() {
    let ids: Vec<Value> = results_of_unpaired_calls()
        .iter()
        .map(|result| result["tool_use_id"].clone())
        .collect();
    assert_eq!(ids, [json!("a"), json!("b")]);
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
        Message::Assistant(AssistantMessage::new(
            provider,
            model,
            StopReason::Stop,
            vec![
                thinking("other", Some("sig-2")),
                ContentBlock::Text { text: "theirs".into() },
            ],
        ))
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
    assert_eq!(inline["messages"][6]["content"][0]["cache_control"], cache_breakpoint());

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

/// The body of a request with two tools, an output limit, and a call whose id the API
/// does not take as it is.
fn body_with_tools() -> Value {
    let tool = |name: &str| ToolDef {
        name: name.into(),
        description: format!("{name} things"),
        parameters: json!({"type": "object", "properties": {}}),
    };
    build_body(
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
    )
}

#[test]
fn a_requests_output_limit_is_sent() {
    assert_eq!(body_with_tools()["max_tokens"], json!(500));
}

#[test]
fn tools_are_declared_with_the_last_one_a_cache_breakpoint() {
    let body = body_with_tools();
    assert_eq!(
        body["tools"],
        json!([
            {"name": "read", "description": "read things", "input_schema": {"type": "object", "properties": {}}},
            {"name": "write", "description": "write things", "input_schema": {"type": "object", "properties": {}},
             "cache_control": {"type": "ephemeral"}},
        ])
    );
}

#[test]
fn call_ids_take_the_form_the_api_takes() {
    let body = body_with_tools();
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

/// The body of a request at `reasoning` to a provider whose extra fields fix what a level
/// sets, `thinking.type` and `output_config.effort`, beside a setting no level sets.
fn with_fixed_reasoning(reasoning: Option<Reasoning>) -> Value {
    let mut extra_fields = Map::new();
    extra_fields.insert("thinking".into(), json!({"type": "enabled", "block_binding": {}}));
    extra_fields.insert("output_config".into(), json!({"effort": "max"}));
    let options = BodyOptions {
        extra_fields,
        ..BodyOptions::default()
    };
    build_body(
        &Request {
            reasoning,
            ..request(vec![Message::user("hi")])
        },
        &options,
        "p",
    )
}

#[test]
fn a_request_set_to_off_sends_no_fixed_effort() {
    let off = with_fixed_reasoning(Some(Reasoning::Off));
    assert_eq!(off["thinking"], json!({"block_binding": {}, "type": "disabled"}));
    // Nothing else was in it, so the object goes with the effort.
    assert!(off.get("output_config").is_none(), "{off}");
}

#[test]
fn a_request_without_a_level_sends_the_fixed_reasoning_fields() {
    let unset = with_fixed_reasoning(None);
    assert_eq!(unset["thinking"], json!({"type": "enabled", "block_binding": {}}));
    assert_eq!(unset["output_config"], json!({"effort": "max"}));
}

#[test]
fn a_requests_reasoning_level_wins_over_the_fixed_fields() {
    let high = with_fixed_reasoning(Some(Reasoning::High));
    assert_eq!(
        high["thinking"],
        json!({"block_binding": {}, "type": "adaptive", "display": "summarized"})
    );
    assert_eq!(high["output_config"], json!({"effort": "high"}));
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
        json!({"format": "anthropic-messages", "version": 3, "systemUpdates": "collapse"}),
        json!({"format": "anthropic-messages", "version": 2, "systemUpdates": "sideways"}),
        json!({"format": "anthropic-messages", "version": 2, "systemUpdates": "collapse", "extra": 1}),
        json!({"format": "anthropic-messages", "version": 2}),
        json!({"format": "anthropic-messages", "version": 2, "systemUpdates": "collapse", "extraFields": {"model": "x"}}),
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
        json!({"format": "anthropic-messages", "version": 2, "systemUpdates": "inline"})
    );
}

/// An image with a stand-in for its data, which the conversion does not read.
fn png() -> Image {
    Image {
        mime_type: "image/png".into(),
        data: "AAAA".into(),
    }
}

/// The block of [`png`].
fn png_block() -> Value {
    json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}})
}

/// The result of the call `id`, failed or not: `content` and an image.
fn image_result(id: &str, content: &str, is_error: bool) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: id.into(),
        tool_name: "read".into(),
        content: content.into(),
        images: vec![png()],
        is_error,
        timestamp: 0,
        duration_ms: None,
        details: None,
        usage: None,
    })
}

/// `request(messages)` to a model that takes images.
fn seeing(messages: Vec<Message>) -> Request {
    Request {
        takes_images: true,
        ..request(messages)
    }
}

#[test]
fn a_user_message_sends_its_images_before_its_text() {
    let body = body_of(
        &seeing(vec![Message::user_with_images("what is this?", vec![png()])]),
        SystemUpdates::Collapse,
    );
    let content = &body["messages"][0]["content"];
    assert_eq!(content[0], png_block());
    assert_eq!(content[1]["text"], "what is this?");
    assert_eq!(content.as_array().unwrap().len(), 2);
}

#[test]
fn a_result_carries_its_images_and_a_failed_one_sends_them_after_the_results() {
    let body = body_of(
        &seeing(vec![
            Message::user("look"),
            answer(vec![call("a"), call("b")]),
            image_result("a", "Read image file [image/png]", false),
            image_result("b", "cannot", true),
        ]),
        SystemUpdates::Collapse,
    );
    let turn = body["messages"][2]["content"].as_array().unwrap();
    assert_eq!(turn.len(), 4, "{body}");
    assert_eq!(
        turn[0],
        json!({"type": "tool_result", "tool_use_id": "a", "content": [
            {"type": "text", "text": "Read image file [image/png]"}, png_block()
        ], "is_error": false})
    );
    assert_eq!(
        turn[1],
        json!({"type": "tool_result", "tool_use_id": "b",
            "content": "cannot\n(image attached after the tool results)", "is_error": true})
    );
    assert_eq!(
        turn[2],
        json!({"type": "text", "text": "Images from the error result of tool call b (read):"})
    );
    assert_eq!(turn[3]["source"], png_block()["source"]);
}

#[test]
fn a_model_that_takes_no_images_gets_a_note_in_their_place() {
    let body = body_of(
        &request(vec![
            Message::user_with_images("what is this?", vec![png()]),
            answer(vec![call("a")]),
            image_result("a", "Read image file [image/png]", false),
        ]),
        SystemUpdates::Collapse,
    );
    assert_eq!(
        body["messages"][0]["content"][0]["text"],
        "what is this?\n(image omitted: model does not support images)"
    );
    assert_eq!(
        body["messages"][2]["content"][0]["content"],
        "Read image file [image/png]\n(tool image omitted: model does not support images)"
    );
}
