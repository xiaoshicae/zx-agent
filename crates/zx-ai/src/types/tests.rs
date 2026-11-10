use serde_json::json;

use super::*;
use crate::{SectionChange, SystemMessage};

#[test]
fn reads_older_uncorrelated_deltas() {
    let delta: Delta = serde_json::from_value(json!({"kind": "tool_call_args", "text": "{}"})).unwrap();
    assert_eq!(
        delta,
        Delta::ToolCallArgs {
            id: String::new(),
            text: "{}".into()
        }
    );
}

#[test]
fn provider_error_metadata_is_optional_and_round_trips() {
    let mut message = AssistantMessage::failure("p", "m", StopReason::Error, "rate limited");
    let legacy = serde_json::to_value(&message).unwrap();
    assert!(legacy.get("providerError").is_none());
    assert_eq!(
        serde_json::from_value::<AssistantMessage>(legacy)
            .unwrap()
            .provider_error,
        None
    );
    message.provider_error = Some(ProviderError::Http {
        status: 429,
        code: Some("rate_limit_exceeded".into()),
        retry_after_ms: Some(2000),
    });
    let encoded = serde_json::to_value(&message).unwrap();
    assert_eq!(encoded["providerError"]["retryAfterMs"], 2000);
    assert_eq!(serde_json::from_value::<AssistantMessage>(encoded).unwrap(), message);
}

fn assistant() -> AssistantMessage {
    AssistantMessage {
        content: vec![
            ContentBlock::Thinking {
                thinking: "plan".into(),
                signature: Some("reasoning_content".into()),
            },
            ContentBlock::Text {
                text: "Checking.".into(),
            },
            ContentBlock::ToolCall(ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: json!({"command": "ls"}),
                arguments_error: None,
            }),
        ],
        provider: "p".into(),
        model: "m".into(),
        usage: Usage {
            input: 3,
            output: 2,
            cache_read: 1,
            total_tokens: 5,
            ..Usage::default()
        },
        stop_reason: StopReason::ToolUse,
        error_message: None,
        provider_error: None,
        timestamp: 7,
        duration_ms: None,
        response_id: None,
        response_model: None,
    }
}

#[test]
fn tool_result_details_are_recorded_only_when_present() {
    let result = ToolResultMessage {
        tool_call_id: "c1".into(),
        tool_name: "bash".into(),
        content: "ok".into(),
        is_error: false,
        timestamp: 4,
        duration_ms: None,
        details: Some(json!({"exitCode": 0})),
        usage: None,
    };
    let recorded = json!({
        "role": "toolResult", "toolCallId": "c1", "toolName": "bash", "content": "ok",
        "isError": false, "timestamp": 4, "details": {"exitCode": 0}
    });
    let message = Message::ToolResult(result);
    assert_eq!(serde_json::to_value(&message).unwrap(), recorded);
    assert_eq!(serde_json::from_value::<Message>(recorded).unwrap(), message);
}

#[test]
fn messages_keep_the_session_file_format() {
    // Session files store messages in exactly this shape; changing it breaks existing files.
    let cases = [
        (
            Message::System(SystemMessage {
                sections: vec![SectionChange {
                    name: "preamble".into(),
                    content: Some("Do it.".into()),
                }],
                timestamp: 1,
            }),
            json!({"role": "system", "sections": [{"name": "preamble", "content": "Do it."}], "timestamp": 1}),
        ),
        (
            Message::User {
                content: "go".into(),
                timestamp: 2,
                kind: UserMessageKind::Prompt,
            },
            json!({"role": "user", "content": "go", "timestamp": 2}),
        ),
        (
            Message::Assistant(assistant()),
            json!({
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "plan", "signature": "reasoning_content"},
                    {"type": "text", "text": "Checking."},
                    {"type": "toolCall", "id": "c1", "name": "bash", "arguments": {"command": "ls"}}
                ],
                "provider": "p",
                "model": "m",
                "usage": {"input": 3, "output": 2, "cacheRead": 1, "totalTokens": 5},
                "stopReason": "toolUse",
                "timestamp": 7
            }),
        ),
        (
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "c1".into(),
                tool_name: "bash".into(),
                content: "ok".into(),
                is_error: false,
                timestamp: 8,
                duration_ms: Some(4),
                details: None,
                usage: None,
            }),
            json!({
                "role": "toolResult", "toolCallId": "c1", "toolName": "bash", "content": "ok",
                "isError": false, "timestamp": 8, "durationMs": 4
            }),
        ),
    ];
    for (message, expected) in cases {
        assert_eq!(serde_json::to_value(&message).unwrap(), expected);
        assert_eq!(serde_json::from_value::<Message>(expected).unwrap(), message);
    }
}

#[test]
fn tool_call_blocks_keep_the_session_file_format_with_and_without_an_arguments_error() {
    // Compared as text, so the order of the fields counts too.
    let cases = [
        (
            None,
            r#"{"type":"toolCall","id":"c1","name":"bash","arguments":{"command":"ls"}}"#,
        ),
        (
            Some("expected value".to_string()),
            r#"{"type":"toolCall","id":"c1","name":"bash","arguments":{"command":"ls"},"argumentsError":"expected value"}"#,
        ),
    ];
    for (arguments_error, recorded) in cases {
        let block = ContentBlock::ToolCall(ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: json!({"command": "ls"}),
            arguments_error,
        });
        assert_eq!(serde_json::to_string(&block).unwrap(), recorded);
        assert_eq!(serde_json::from_str::<ContentBlock>(recorded).unwrap(), block);
    }
}

#[test]
fn tool_result_usage_is_recorded_only_when_present() {
    let message = Message::ToolResult(ToolResultMessage {
        tool_call_id: "c2".into(),
        tool_name: "ask_model".into(),
        content: "yes".into(),
        is_error: false,
        timestamp: 9,
        duration_ms: None,
        details: None,
        usage: Some(Usage {
            input: 20,
            output: 4,
            cache_read: 0,
            cache_write: 0,
            reasoning: 0,
            total_tokens: 24,
            total_cost: Some(0.001),
        }),
    });
    let recorded = json!({
        "role": "toolResult", "toolCallId": "c2", "toolName": "ask_model", "content": "yes",
        "isError": false, "timestamp": 9,
        "usage": {"input": 20, "output": 4, "cacheRead": 0, "totalTokens": 24, "totalCost": 0.001}
    });
    assert_eq!(serde_json::to_value(&message).unwrap(), recorded);
    assert_eq!(serde_json::from_value::<Message>(recorded).unwrap(), message);
}

#[test]
fn continuation_kind_round_trips_and_survives_text_edits() {
    let message = Message::continuation("继续");
    let json = serde_json::to_value(&message).unwrap();
    assert_eq!(json["kind"], "continueAnswer");
    assert_eq!(serde_json::from_value::<Message>(json).unwrap(), message);
    assert!(
        matches!(message.with_replaced_text("接着说"), Message::User { kind: UserMessageKind::ContinueAnswer, content, .. } if content == "接着说")
    );
    let legacy: Message = serde_json::from_value(json!({"role": "user", "content": "old", "timestamp": 1})).unwrap();
    assert!(matches!(
        legacy,
        Message::User {
            kind: UserMessageKind::Prompt,
            ..
        }
    ));
}

#[test]
fn text_and_tool_calls_read_the_blocks_in_order() {
    let message = assistant();
    assert_eq!(message.text(), "Checking.");
    let ids: Vec<String> = message.tool_calls().into_iter().map(|call| call.id).collect();
    assert_eq!(ids, ["c1"]);
}

#[test]
fn usage_add_sums_every_count() {
    let mut total = assistant().usage;
    total.add(&assistant().usage);
    assert_eq!(
        total,
        Usage {
            input: 6,
            output: 4,
            cache_read: 2,
            total_tokens: 10,
            ..Usage::default()
        }
    );
}

#[test]
fn usage_totals_saturate_instead_of_overflowing() {
    let mut total = Usage {
        input: u64::MAX - 1,
        output: 1,
        cache_read: 0,
        total_tokens: u64::MAX,
        ..Usage::default()
    };
    total.add(&Usage {
        input: 5,
        output: u64::MAX,
        cache_read: 3,
        total_tokens: 1,
        ..Usage::default()
    });
    assert_eq!(
        total,
        Usage {
            input: u64::MAX,
            output: u64::MAX,
            cache_read: 3,
            total_tokens: u64::MAX,
            ..Usage::default()
        }
    );
}

#[test]
fn usage_sums_cache_writes_reasoning_and_known_costs() {
    let mut total = Usage {
        cache_write: 1,
        reasoning: 2,
        total_cost: None,
        ..Usage::default()
    };
    total.add(&Usage {
        cache_write: 3,
        reasoning: 4,
        total_cost: Some(0.5),
        ..Usage::default()
    });
    total.add(&Usage {
        total_cost: Some(0.25),
        ..Usage::default()
    });
    assert_eq!(
        (total.cache_write, total.reasoning, total.total_cost),
        (4, 6, Some(0.75))
    );
    // Counts a server did not report stay out of the JSON.
    let json = serde_json::to_value(Usage {
        input: 1,
        output: 2,
        cache_read: 0,
        total_tokens: 3,
        ..Usage::default()
    })
    .unwrap();
    assert_eq!(json, json!({"input": 1, "output": 2, "cacheRead": 0, "totalTokens": 3}));
    let json = serde_json::to_value(total).unwrap();
    assert_eq!(
        (json["cacheWrite"].as_u64(), json["reasoning"].as_u64()),
        (Some(4), Some(6))
    );
    assert_eq!(json["totalCost"].as_f64(), Some(0.75));
}

#[test]
fn prices_apply_per_million_tokens_with_the_cache_priced_separately() {
    let cost = ModelCost {
        input: 1.0,
        output: 4.0,
        cache_read: 0.1,
        cache_write: 2.0,
    };
    let usage = Usage {
        input: 1_000_000,
        output: 500_000,
        cache_read: 200_000,
        cache_write: 100_000,
        reasoning: 100_000,
        total_tokens: 1_500_000,
        total_cost: None,
    };
    // 700k fresh at 1 + 200k cached at 0.1 + 100k written at 2 + 500k out at 4, per million.
    assert!((cost.of(&usage) - (0.7 + 0.02 + 0.2 + 2.0)).abs() < 1e-9);
    // A cache count above the input never underflows.
    let odd = Usage {
        input: 10,
        cache_read: 20,
        ..Usage::default()
    };
    assert!((cost.of(&odd) - 20.0 * 0.1 / 1_000_000.0).abs() < 1e-12);
}
