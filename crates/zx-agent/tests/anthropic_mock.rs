//! The Anthropic Messages API provider over real HTTP against a local fake server: the request
//! it sends, thinking sent back with its signature, tool results, the retry without thinking
//! the API asks for, and how failures are classified. No network or API key needed.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use zx_agent::ai::anthropic::{AnthropicConfig, AnthropicMessages, DEFAULT_API_VERSION};
use zx_agent::ai::{PREAMBLE, ProviderError, ProviderErrorKind, SystemPrompt};
use zx_agent::builtin::tools::BashTool;
use zx_agent::events::RunOutcome;
use zx_testing::mock_server::{MockServer, Reply};
use zx_testing::{RunnerSetup, config, fast_retry};

/// A runner whose Anthropic provider talks to `server`, with a bash tool and a one-line
/// system prompt.
fn setup(server: &MockServer) -> RunnerSetup {
    let provider = AnthropicMessages::new(AnthropicConfig {
        // The fake serves any path; the provider adds `/v1/messages` to the address.
        base_url: server.base_url.trim_end_matches("/v1").to_string(),
        api_key: "test-key".into(),
        use_system_proxy: false,
        idle_timeout: Duration::from_secs(10),
        ..AnthropicConfig::default()
    })
    .unwrap();
    let mut config = config(Arc::new(provider), vec![Arc::new(BashTool::default())]);
    config.model = "claude-test".into();
    config.system_prompt = SystemPrompt::default().with_section(PREAMBLE, "You are a test.");
    config.max_tokens = Some(256);
    RunnerSetup::new(config)
}

fn started() -> Vec<Value> {
    vec![
        json!({"type": "message_start", "message": {"id": "msg_1", "model": "claude-test",
        "usage": {"input_tokens": 20, "cache_read_input_tokens": 300, "cache_creation_input_tokens": 10, "output_tokens": 1}}}),
    ]
}

fn text(index: u64, text: &str) -> Vec<Value> {
    vec![
        json!({"type": "content_block_start", "index": index, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": index, "delta": {"type": "text_delta", "text": text}}),
        json!({"type": "content_block_stop", "index": index}),
    ]
}

fn thinking(index: u64, thinking: &str, signature: &str) -> Vec<Value> {
    vec![
        json!({"type": "content_block_start", "index": index, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
        json!({"type": "content_block_delta", "index": index, "delta": {"type": "thinking_delta", "thinking": thinking}}),
        json!({"type": "content_block_delta", "index": index, "delta": {"type": "signature_delta", "signature": signature}}),
        json!({"type": "content_block_stop", "index": index}),
    ]
}

fn bash(index: u64, id: &str, command: &str) -> Vec<Value> {
    let input = json!({ "command": command }).to_string();
    vec![
        json!({"type": "content_block_start", "index": index, "content_block": {"type": "tool_use", "id": id, "name": "bash", "input": {}}}),
        json!({"type": "content_block_delta", "index": index, "delta": {"type": "input_json_delta", "partial_json": input}}),
        json!({"type": "content_block_stop", "index": index}),
    ]
}

fn ended(reason: &str) -> Vec<Value> {
    vec![
        json!({"type": "message_delta", "delta": {"stop_reason": reason}, "usage": {"output_tokens": 7}}),
        json!({"type": "message_stop"}),
    ]
}

/// A streamed answer made of `parts`, in order.
fn answer(parts: Vec<Vec<Value>>) -> Reply {
    Reply::Sse(parts.into_iter().flatten().collect())
}

fn error_reply(status: u16, headers: &str, kind: &str, message: &str) -> Reply {
    let body = json!({"type": "error", "error": {"type": kind, "message": message}}).to_string();
    Reply::error(status, headers, &body)
}

#[tokio::test]
async fn a_tool_round_trip_sends_thinking_back_with_its_signature() {
    let server = MockServer::start(vec![
        answer(vec![
            started(),
            thinking(0, "I should look.", "sig-1"),
            bash(1, "toolu_1", "echo hi"),
            ended("tool_use"),
        ]),
        answer(vec![started(), text(0, "done"), ended("end_turn")]),
    ])
    .await;
    let (runner, _) = setup(&server).build().await;

    let report = runner.run("check").await;

    assert_eq!(report.outcome, RunOutcome::Completed);
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let headers = &requests[0].headers;
    assert_eq!(headers.lines().next(), Some("post /v1/messages"), "{headers}");
    assert!(headers.contains("x-api-key: test-key"), "{headers}");
    assert!(
        headers.contains(&format!("anthropic-version: {DEFAULT_API_VERSION}")),
        "{headers}"
    );
    let first = &requests[0].body;
    assert_eq!(first["model"], json!("claude-test"));
    assert_eq!(first["max_tokens"], json!(256));
    assert_eq!(first["system"][0]["text"], json!("You are a test."));
    assert_eq!(first["tools"][0]["name"], json!("bash"));
    assert_eq!(first["messages"][0]["content"][0]["text"], json!("check"));

    let second = &requests[1].body;
    assert_eq!(
        second["messages"][1]["content"][0],
        json!({"type": "thinking", "thinking": "I should look.", "signature": "sig-1"})
    );
    assert_eq!(second["messages"][1]["content"][1]["id"], json!("toolu_1"));
    let result = &second["messages"][2]["content"][0];
    assert_eq!(
        (&result["type"], &result["tool_use_id"]),
        (&json!("tool_result"), &json!("toolu_1"))
    );
    assert!(result["content"].as_str().unwrap().contains("hi"), "{result}");

    // The cached prompt tokens count as input, as every provider reports them.
    let usage = &report.usage;
    assert_eq!((usage.input, usage.cache_read, usage.cache_write), (660, 600, 20));
}

#[tokio::test]
async fn thinking_bound_to_another_conversation_fails_the_request_as_the_api_answered() {
    let refused = error_reply(
        400,
        "",
        "invalid_request_error",
        "messages.1.content.0: Invalid `signature` in `thinking` block. The block is bound to a different conversation.",
    );
    let server = MockServer::start(vec![
        answer(vec![
            started(),
            thinking(0, "pondering", "sig-1"),
            text(1, "first"),
            ended("end_turn"),
        ]),
        refused,
    ])
    .await;
    let (runner, _) = setup(&server).build().await;
    runner.run("one").await;

    let report = runner.run("two").await;

    // Nothing leaves the thinking out unrecorded: the API drops it only when the request's
    // `thinking.block_binding` asks it to.
    assert_eq!((report.outcome, report.retries), (RunOutcome::Failed, 0));
    let error = report.error.unwrap_or_default();
    assert!(error.contains("bound to a different conversation"), "{error}");
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].body["messages"][1]["content"][0]["type"], json!("thinking"));
}

#[tokio::test]
async fn an_overloaded_api_is_unavailable_with_its_wait_and_a_refusal_is_refused() {
    let server = MockServer::start(vec![error_reply(
        529,
        "retry-after: 2\r\n",
        "overloaded_error",
        "Overloaded",
    )])
    .await;
    let (runner, _) = RunnerSetup {
        retry: fast_retry(0),
        ..setup(&server)
    }
    .build()
    .await;

    let report = runner.run("check").await;

    assert_eq!(report.outcome, RunOutcome::Failed);
    assert_eq!(
        runner.provider_error(),
        ProviderError {
            kind: ProviderErrorKind::Unavailable,
            status: Some(529),
            code: Some("overloaded_error".into()),
            retry_after_ms: Some(2000),
        }
    );
    assert!(report.error.unwrap().contains("HTTP 529: Overloaded"));

    let server = MockServer::start(vec![Reply::Sse(vec![
        started()[0].clone(),
        json!({"type": "message_delta", "delta": {"stop_reason": "refusal", "stop_details": {"type": "refusal", "category": "cyber"}}}),
        json!({"type": "message_stop"}),
    ])])
    .await;
    let (runner, _) = setup(&server).build().await;

    let report = runner.run("check").await;

    assert_eq!(report.outcome, RunOutcome::Failed);
    assert_eq!(runner.provider_error().kind, ProviderErrorKind::Refused);
    assert!(report.error.unwrap().contains("the model refused the request (cyber)"));
}
