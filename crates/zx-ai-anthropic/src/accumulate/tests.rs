use serde_json::json;

use super::*;

fn apply_all(acc: &mut Accumulator, events: &[Value]) -> Vec<Delta> {
    events
        .iter()
        .flat_map(|event| match acc.apply(event) {
            Ok(deltas) => deltas,
            Err(_) => panic!("unexpected failure at {event}"),
        })
        .collect()
}

fn ended(reason: &str) -> Value {
    json!({"type": "message_delta", "delta": {"stop_reason": reason}, "usage": {"output_tokens": 12}})
}

#[test]
fn a_text_answer_streams_its_deltas_and_counts_cached_prompt_tokens_as_input() {
    let mut acc = Accumulator::new("anthropic", "claude-opus-5-5", 1);
    let deltas = apply_all(
        &mut acc,
        &[
            json!({"type": "message_start", "message": {"id": "msg_1", "model": "claude-opus-5-5-20261101",
                "usage": {"input_tokens": 10, "cache_read_input_tokens": 100, "cache_creation_input_tokens": 5, "output_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "ping"}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hel"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "lo"}}),
            json!({"type": "content_block_stop", "index": 0}),
            ended("end_turn"),
            json!({"type": "message_stop"}),
        ],
    );
    assert_eq!(
        deltas,
        [Delta::Text { text: "Hel".into() }, Delta::Text { text: "lo".into() }]
    );
    let message = acc.finish().unwrap();
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.text(), "Hello");
    assert_eq!(message.response_id.as_deref(), Some("msg_1"));
    assert_eq!(message.response_model.as_deref(), Some("claude-opus-5-5-20261101"));
    let usage = message.usage;
    assert_eq!(
        (
            usage.input,
            usage.cache_read,
            usage.cache_write,
            usage.output,
            usage.total_tokens
        ),
        (115, 100, 5, 12, 127)
    );
}

#[test]
fn thinking_keeps_its_signature_and_tool_calls_their_streamed_arguments() {
    let mut acc = Accumulator::new("anthropic", "m", 1);
    let deltas = apply_all(
        &mut acc,
        &[
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Let me look."}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig"}}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "redacted_thinking", "data": "opaque"}}),
            // A server tool's block is not kept, nor are its deltas.
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "server_tool_use", "id": "srv", "name": "web_search"}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{}"}}),
            json!({"type": "content_block_start", "index": 3, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "bash", "input": {}}}),
            json!({"type": "content_block_delta", "index": 3, "delta": {"type": "input_json_delta", "partial_json": "{\"command\":"}}),
            json!({"type": "content_block_delta", "index": 3, "delta": {"type": "input_json_delta", "partial_json": "\"ls\"}"}}),
            ended("tool_use"),
        ],
    );
    assert_eq!(
        deltas,
        [
            Delta::Thinking {
                text: "Let me look.".into()
            },
            Delta::ToolCallStart {
                id: "toolu_1".into(),
                name: "bash".into()
            },
            Delta::ToolCallArgs {
                id: "toolu_1".into(),
                text: "{\"command\":".into()
            },
            Delta::ToolCallArgs {
                id: "toolu_1".into(),
                text: "\"ls\"}".into()
            },
        ]
    );
    let message = acc.finish().unwrap();
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(
        message.content,
        [
            ContentBlock::Thinking {
                thinking: "Let me look.".into(),
                signature: Some("sig".into())
            },
            ContentBlock::Thinking {
                thinking: String::new(),
                signature: Some("redacted:opaque".into())
            },
            ContentBlock::ToolCall(ToolCall {
                id: "toolu_1".into(),
                name: "bash".into(),
                arguments: serde_json::json!({"command": "ls"}),
                arguments_error: None
            }),
        ]
    );
}

#[test]
fn a_cut_off_answer_is_long_a_full_window_an_overflow_and_a_refusal_a_failure_of_its_own_kind() {
    let finished = |reason: &str, details: Value| {
        let mut acc = Accumulator::new("anthropic", "m", 1);
        acc.apply(&json!({"type": "message_delta", "delta": {"stop_reason": reason, "stop_details": details}}))
            .unwrap_or_else(|_| panic!("unexpected failure"));
        acc.finish().unwrap()
    };
    assert_eq!(finished("max_tokens", Value::Null).stop_reason, StopReason::Length);
    assert_eq!(finished("pause_turn", Value::Null).stop_reason, StopReason::Stop);

    // Continuing would not fit either: the conversation is compacted, when it can be.
    let full = finished("model_context_window_exceeded", Value::Null);
    assert_eq!(full.stop_reason, StopReason::Error);
    let details = full.provider_error.unwrap();
    assert_eq!(
        (details.kind, details.code.as_deref()),
        (
            ProviderErrorKind::ContextOverflow,
            Some("model_context_window_exceeded")
        )
    );

    let refused = finished(
        "refusal",
        json!({"type": "refusal", "category": "cyber", "explanation": "Declined."}),
    );
    assert_eq!(refused.stop_reason, StopReason::Error);
    assert_eq!(
        refused.error_message.as_deref(),
        Some("the model refused the request (cyber): Declined.")
    );
    let details = refused.provider_error.unwrap();
    assert_eq!(
        (details.kind, details.code.as_deref()),
        (ProviderErrorKind::Refused, Some("refusal"))
    );

    // A reason added to the API since is not taken for success, and not retried.
    let unknown = finished("sensitive", Value::Null);
    assert_eq!(unknown.stop_reason, StopReason::Error);
    assert_eq!(unknown.provider_error.unwrap().kind, ProviderErrorKind::Refused);
}

#[test]
fn a_stream_without_a_stop_reason_or_with_an_error_event_fails() {
    let mut acc = Accumulator::new("anthropic", "m", 1);
    apply_all(
        &mut acc,
        &[json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": "par"}})],
    );
    assert_eq!(acc.finish().unwrap_err(), "stream ended without a stop_reason");
    assert_eq!(
        acc.failure(StopReason::Error, "cut", None).text(),
        "par",
        "what streamed is kept"
    );

    let error = json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}});
    match acc.apply(&error) {
        Err(Failure::Error { details, .. }) => assert_eq!(details.kind, ProviderErrorKind::Unavailable),
        _ => panic!("an error event fails the response"),
    }
}
