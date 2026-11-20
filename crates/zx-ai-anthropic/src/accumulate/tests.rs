use serde_json::json;

use super::*;

fn apply_all(acc: &mut Accumulator, events: &[Value]) -> Vec<Delta> {
    events
        .iter()
        .flat_map(|event| {
            acc.apply(event)
                .unwrap_or_else(|failure| panic!("{failure:?} at {event}"))
        })
        .collect()
}

fn ended(reason: &str) -> Value {
    json!({"type": "message_delta", "delta": {"stop_reason": reason}, "usage": {"output_tokens": 12}})
}

#[test]
fn an_empty_id_or_model_names_nothing() {
    let mut acc = Accumulator::new("anthropic", "claude-opus-5-5", 1);
    apply_all(
        &mut acc,
        &[
            json!({"type": "message_start", "message": {"id": "", "model": "", "usage": {"input_tokens": 1}}}),
            ended("end_turn"),
        ],
    );
    let message = acc.finish().unwrap();
    assert_eq!((message.response_id, message.response_model), (None, None));
}

/// A streamed text answer: its deltas and the message it makes.
fn text_answer() -> (Vec<Delta>, AssistantMessage) {
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
    (deltas, acc.finish().unwrap())
}

#[test]
fn a_text_answer_streams_its_deltas() {
    let (deltas, message) = text_answer();
    assert_eq!(
        deltas,
        [Delta::Text { text: "Hel".into() }, Delta::Text { text: "lo".into() }]
    );
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.text(), "Hello");
}

#[test]
fn the_response_id_and_the_model_that_answered_are_kept() {
    let (_, message) = text_answer();
    assert_eq!(message.response_id.as_deref(), Some("msg_1"));
    assert_eq!(message.response_model.as_deref(), Some("claude-opus-5-5-20261101"));
}

#[test]
fn cached_prompt_tokens_count_as_input() {
    let usage = text_answer().1.usage;
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

/// An answer that thinks, holds a redacted thinking block and a server tool's block, then
/// calls a tool: its deltas and the message it makes.
fn thinking_then_a_call() -> (Vec<Delta>, AssistantMessage) {
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
    (deltas, acc.finish().unwrap())
}

#[test]
fn thinking_keeps_its_signature() {
    let (deltas, message) = thinking_then_a_call();
    assert_eq!(
        deltas[0],
        Delta::Thinking {
            text: "Let me look.".into()
        }
    );
    assert_eq!(
        message.content[..2],
        [
            ContentBlock::Thinking {
                thinking: "Let me look.".into(),
                signature: Some("sig".into())
            },
            ContentBlock::Thinking {
                thinking: String::new(),
                signature: Some("redacted:opaque".into())
            },
        ]
    );
}

#[test]
fn a_tool_call_keeps_its_streamed_arguments() {
    let (deltas, message) = thinking_then_a_call();
    assert_eq!(
        deltas[1..],
        [
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
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(
        message.content[2..],
        [ContentBlock::ToolCall(ToolCall {
            id: "toolu_1".into(),
            name: "bash".into(),
            arguments: json!({"command": "ls"}),
            arguments_error: None
        })]
    );
}

#[test]
fn a_server_tools_block_is_not_kept() {
    let (deltas, message) = thinking_then_a_call();
    assert_eq!((deltas.len(), message.content.len()), (4, 3));
}

/// The message of a response that ended with `reason`, and `details` as its `stop_details`.
fn finished(reason: &str, details: &Value) -> AssistantMessage {
    let mut acc = Accumulator::new("anthropic", "m", 1);
    acc.apply(&json!({"type": "message_delta", "delta": {"stop_reason": reason, "stop_details": details}}))
        .unwrap_or_else(|failure| panic!("{failure:?}"));
    acc.finish().unwrap()
}

#[test]
fn an_answer_cut_off_at_the_output_limit_is_long() {
    assert_eq!(finished("max_tokens", &Value::Null).stop_reason, StopReason::Length);
}

#[test]
fn a_paused_turn_ends_the_answer() {
    assert_eq!(finished("pause_turn", &Value::Null).stop_reason, StopReason::Stop);
}

#[test]
fn a_full_context_window_is_an_overflow() {
    // Continuing would not fit either: the conversation is compacted, when it can be.
    let full = finished("model_context_window_exceeded", &Value::Null);
    assert_eq!(full.stop_reason, StopReason::Error);
    let provider_error = full.provider_error.unwrap();
    assert_eq!(
        (provider_error.kind, provider_error.code.as_deref()),
        (
            ProviderErrorKind::ContextOverflow,
            Some("model_context_window_exceeded")
        )
    );
}

#[test]
fn a_refusal_is_a_failure_of_its_own_kind_with_the_apis_explanation() {
    let refused = finished(
        "refusal",
        &json!({"type": "refusal", "category": "cyber", "explanation": "Declined."}),
    );
    assert_eq!(refused.stop_reason, StopReason::Error);
    assert_eq!(
        refused.error_message.as_deref(),
        Some("the model refused the request (cyber): Declined.")
    );
    let provider_error = refused.provider_error.unwrap();
    assert_eq!(
        (provider_error.kind, provider_error.code.as_deref()),
        (ProviderErrorKind::Refused, Some("refusal"))
    );
}

#[test]
fn a_stop_reason_added_to_the_api_since_is_a_refusal() {
    // Not taken for success, and so not retried either.
    let unknown = finished("sensitive", &Value::Null);
    assert_eq!(unknown.stop_reason, StopReason::Error);
    assert_eq!(unknown.provider_error.unwrap().kind, ProviderErrorKind::Refused);
}

/// The start of a response whose request the API changed as `listed` says.
fn started_with(listed: &Value) -> Value {
    json!({"type": "message_start", "message": {"id": "msg_1", "model": "m", "usage": {"input_tokens": 1},
        "input_transformations": listed}})
}

/// The changes to the request that the answer made of `events` records.
fn transformations(events: &[Value]) -> Vec<InputTransformation> {
    let mut acc = Accumulator::new("anthropic", "m", 1);
    apply_all(&mut acc, events);
    acc.finish().unwrap().input_transformations
}

fn dropped(path: &str, reason: &str) -> InputTransformation {
    InputTransformation {
        kind: "thinking_dropped".into(),
        path: Some(path.into()),
        reason: Some(reason.into()),
    }
}

#[test]
fn the_changes_the_api_made_to_the_request_are_recorded_in_its_words() {
    let listed = json!([
        {"type": "thinking_dropped", "path": "messages.1.content.0", "reason": "prefix_binding_mismatch"},
        // Values added to the API since are kept as they came.
        {"type": "thinking_trimmed", "reason": "something_new"},
    ]);
    assert_eq!(
        transformations(&[started_with(&listed), ended("end_turn")]),
        [
            dropped("messages.1.content.0", "prefix_binding_mismatch"),
            InputTransformation {
                kind: "thinking_trimmed".into(),
                path: None,
                reason: Some("something_new".into())
            },
        ]
    );
}

#[test]
fn a_change_without_a_type_is_passed_over() {
    let listed = json!([{"path": "messages.1.content.0", "reason": "prefix_binding_mismatch"}]);
    assert_eq!(transformations(&[started_with(&listed), ended("end_turn")]), []);
}

#[test]
fn the_list_after_a_fallback_replaces_the_first() {
    let first =
        json!([{"type": "thinking_dropped", "path": "messages.1.content.0", "reason": "prefix_binding_mismatch"}]);
    let after_fallback = json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
        "input_transformations": [{"type": "thinking_dropped", "path": "messages.3.content.0", "reason": "model_binding_mismatch"}]});
    assert_eq!(
        transformations(&[started_with(&first), after_fallback]),
        [dropped("messages.3.content.0", "model_binding_mismatch")]
    );
}

/// An answer that streamed some text and then ended without a stop reason.
fn cut_off_text() -> Accumulator {
    let mut acc = Accumulator::new("anthropic", "m", 1);
    apply_all(
        &mut acc,
        &[json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": "par"}})],
    );
    acc
}

#[test]
fn a_stream_without_a_stop_reason_does_not_finish() {
    assert_eq!(
        cut_off_text().finish().unwrap_err(),
        "stream ended without a stop_reason"
    );
}

#[test]
fn a_failed_answer_keeps_what_streamed() {
    assert_eq!(cut_off_text().failure(StopReason::Error, "cut", None).text(), "par");
}

#[test]
fn an_error_event_fails_the_response_as_its_type() {
    let error = json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}});
    let Failure::Error { provider_error, .. } = cut_off_text().apply(&error).unwrap_err() else {
        panic!("an error event fails the response");
    };
    assert_eq!(provider_error.kind, ProviderErrorKind::Unavailable);
}
