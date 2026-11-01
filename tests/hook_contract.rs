//! The JSON contract of hook inputs and outputs: what a hook process or an approval relay
//! reads and writes. Changing a format here breaks those programs.

use std::path::PathBuf;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use zx_agent::agent::{ContextEdit, Decision, HookContext, ToolOutput, TurnDecision, TurnDisposition, TurnSummary};
use zx_agent::ai::{AssistantMessage, Message, StopReason, ToolCall};
use zx_agent::interaction::{Approval, ApprovalRequest};

/// `value` serializes to `expected` and reads back unchanged.
fn round_trip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(value: T, expected: Value) {
    assert_eq!(serde_json::to_value(&value).unwrap(), expected);
    assert_eq!(serde_json::from_value::<T>(expected).unwrap(), value);
}

#[test]
fn decisions_name_the_verdict_and_give_a_reason_to_deny_or_ask() {
    round_trip(Decision::Allow, json!({"decision": "allow"}));
    round_trip(
        Decision::Deny("not on the list".into()),
        json!({"decision": "deny", "reason": "not on the list"}),
    );
    round_trip(
        Decision::Ask("writes files".into()),
        json!({"decision": "ask", "reason": "writes files"}),
    );
}

#[test]
fn turn_decisions_carry_the_text_of_a_continuation_or_follow_up() {
    round_trip(TurnDecision::Default, json!({"decision": "default"}));
    round_trip(TurnDecision::Stop, json!({"decision": "stop"}));
    round_trip(
        TurnDecision::ContinueAnswer("continue".into()),
        json!({"decision": "continue_answer", "text": "continue"}),
    );
    round_trip(
        TurnDecision::FollowUp("now summarize".into()),
        json!({"decision": "follow_up", "text": "now summarize"}),
    );
}

#[test]
fn context_edits_name_the_edit_and_its_fields() {
    let message = Message::user("note");
    let message_json = serde_json::to_value(&message).unwrap();
    round_trip(
        ContextEdit::Append { message },
        json!({"edit": "append", "message": message_json}),
    );
    round_trip(
        ContextEdit::ReplaceText {
            index: 2,
            text: "shorter".into(),
        },
        json!({"edit": "replace_text", "index": 2, "text": "shorter"}),
    );
    round_trip(ContextEdit::Omit { index: 3 }, json!({"edit": "omit", "index": 3}));
}

#[test]
fn approvals_name_their_scope_and_give_a_reason_to_deny() {
    round_trip(Approval::AllowOnce, json!({"approval": "allow_once"}));
    round_trip(Approval::AllowSession, json!({"approval": "allow_session"}));
    round_trip(
        Approval::Deny("not now".into()),
        json!({"approval": "deny", "reason": "not now"}),
    );
}

#[test]
fn tool_output_flags_default_to_false() {
    round_trip(
        ToolOutput {
            content: "done".into(),
            is_error: true,
            terminate: false,
        },
        json!({"content": "done", "isError": true, "terminate": false}),
    );
    assert_eq!(
        serde_json::from_value::<ToolOutput>(json!({"content": "done"})).unwrap(),
        ToolOutput::ok("done")
    );
}

#[test]
fn tool_output_without_content_is_rejected() {
    let error = serde_json::from_value::<ToolOutput>(json!({"isError": true})).unwrap_err();

    assert!(error.to_string().contains("missing field `content`"), "{error}");
}

#[test]
fn approval_requests_carry_the_exact_call() {
    let call = ToolCall {
        id: "c1".into(),
        name: "bash".into(),
        arguments: json!({"command": "ls"}),
        arguments_error: None,
    };
    round_trip(
        ApprovalRequest {
            call,
            reason: "review".into(),
        },
        json!({
            "call": {"id": "c1", "name": "bash", "arguments": {"command": "ls"}},
            "reason": "review"
        }),
    );
}

#[test]
fn a_tool_call_names_its_fields_as_in_messages() {
    round_trip(
        ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: Value::Null,
            arguments_error: Some("expected value".into()),
        },
        json!({"id": "c1", "name": "bash", "arguments": null, "argumentsError": "expected value"}),
    );
}

#[test]
fn the_hook_context_leaves_out_its_cancellation_token() {
    let ctx = HookContext::new(Some("s1".into()), PathBuf::from("/srv"), CancellationToken::new());

    let value = serde_json::to_value(&ctx).unwrap();

    assert_eq!(value, json!({"sessionId": "s1", "runId": ctx.run_id, "cwd": "/srv"}));
}

#[test]
fn a_turn_summary_serializes_for_hook_processes() {
    let mut response = AssistantMessage::failure("p", "m", StopReason::Stop, "");
    response.error_message = None;
    let messages = vec![Message::user("go")];
    let summary = TurnSummary {
        response: &response,
        messages: &messages,
        turn: 2,
        continuations: 1,
        remaining_turns: Some(8),
        disposition: TurnDisposition::Stop,
    };

    let value = serde_json::to_value(&summary).unwrap();

    assert_eq!(value["response"], serde_json::to_value(&response).unwrap());
    assert_eq!(value["messages"], serde_json::to_value(&messages).unwrap());
    assert_eq!(
        (
            &value["turn"],
            &value["continuations"],
            &value["remainingTurns"],
            &value["disposition"]
        ),
        (&json!(2), &json!(1), &json!(8), &json!("stop"))
    );
}
