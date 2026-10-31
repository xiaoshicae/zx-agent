//! The OpenAI-compatible provider over real HTTP against a local fake server: SSE parsing,
//! tool results sent back, retries, timeouts, and cancellation. No network or API key needed.

mod common;

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio_util::sync::CancellationToken;

use common::mock_server::{MockServer, Reply};
use common::{CancelOnFirstDelta, RunnerSetup, config, fast_retry};
use zx_agent::ai::openai::{OpenAiCompatible, OpenAiConfig};
use zx_agent::ai::{Message, PREAMBLE, StopReason, SystemPrompt};
use zx_agent::runner::RunOutcome;
use zx_agent::tools::BashTool;

/// A runner whose provider talks to `server`, with a bash tool and a one-line system prompt.
fn setup(server: &MockServer, idle_timeout: Duration) -> RunnerSetup {
    let provider = OpenAiCompatible::new(OpenAiConfig {
        base_url: server.base_url.clone(),
        api_key: Some("test-key".into()),
        idle_timeout,
        use_system_proxy: false,
        ..OpenAiConfig::default()
    })
    .unwrap();
    let mut config = config(Arc::new(provider), vec![Arc::new(BashTool::default())]);
    config.model = "mock-model".into();
    config.system_prompt = SystemPrompt::default().with_section(PREAMBLE, "You are a test.");
    config.cwd = std::env::temp_dir();
    config.max_tokens = Some(256);
    RunnerSetup::new(config)
}

#[tokio::test]
async fn tool_call_round_trip_over_http() {
    // The arguments arrive in two fragments, as real servers send them.
    let server = MockServer::start(vec![
        Reply::Sse(vec![
            json!({"choices":[{"index":0,"delta":{"role":"assistant","content":"Checking."}}]}),
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"bash","arguments":""}}]}}]}),
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"command\":\"echo zx-mock\"}"}}]}}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":20,"completion_tokens":7,"total_tokens":27}}),
        ]),
        Reply::Sse(vec![
            json!({"choices":[{"index":0,"delta":{"content":"All good."},"finish_reason":"stop"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":40,"completion_tokens":3,"total_tokens":43}}),
        ]),
    ])
    .await;
    let (runner, _) = setup(&server, Duration::from_secs(10)).build();

    let report = runner.run("check", &CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Completed, "{:?}", report.error);
    assert_eq!(report.final_text, "All good.");
    assert_eq!(report.usage.total_tokens, 70);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].headers.contains("authorization: bearer test-key"));
    let first = &requests[0].body;
    assert_eq!(first["model"], "mock-model");
    assert_eq!(first["stream"], true);
    assert_eq!(first["max_tokens"], 256);
    assert_eq!(
        first["messages"][0],
        json!({"role":"system","content":"You are a test."})
    );
    assert_eq!(first["tools"][0]["function"]["name"], "bash");

    let messages = requests[1].body["messages"].as_array().unwrap();
    let assistant = &messages[2];
    assert_eq!(assistant["tool_calls"][0]["id"], "call_1");
    assert_eq!(
        assistant["tool_calls"][0]["function"]["arguments"],
        "{\"command\":\"echo zx-mock\"}"
    );
    assert_eq!(
        messages[3],
        json!({"role":"tool","tool_call_id":"call_1","content":"zx-mock\n[exit code: 0]"})
    );
}

#[tokio::test]
async fn http_errors_are_retried_then_reported() {
    let server = MockServer::start(vec![
        Reply::Status(503, "{\"error\":{\"message\":\"overloaded\"}}"),
        Reply::Status(500, "{\"error\":{\"message\":\"internal\"}}"),
    ])
    .await;
    let (runner, _) = RunnerSetup {
        retry: fast_retry(1),
        ..setup(&server, Duration::from_secs(10))
    }
    .build();

    let report = runner.run("check", &CancellationToken::new()).await;

    assert_eq!((report.outcome, report.retries), (RunOutcome::Error, 1));
    let error = report.error.unwrap_or_default();
    assert!(error.starts_with("HTTP 500:"), "{error}");
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn a_stalled_stream_fails_after_the_idle_timeout() {
    let first_chunk = json!({"choices":[{"index":0,"delta":{"content":"Start"}}]});
    let server = MockServer::start(vec![Reply::Stall(vec![first_chunk])]).await;
    let (runner, _) = setup(&server, Duration::from_secs(1)).build();

    let report = runner.run("check", &CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Error);
    let error = report.error.unwrap_or_default();
    assert!(error.starts_with("stream stalled"), "{error}");
}

#[tokio::test]
async fn cancelling_mid_stream_keeps_the_text_received_so_far() {
    let first_chunk = json!({"choices":[{"index":0,"delta":{"content":"partial"}}]});
    let server = MockServer::start(vec![Reply::Stall(vec![first_chunk])]).await;
    let cancel = CancellationToken::new();
    let runner = setup(&server, Duration::from_secs(10)).build_with(Arc::new(CancelOnFirstDelta(cancel.clone())));

    let report = runner.run("check", &cancel).await;

    assert_eq!(report.outcome, RunOutcome::Aborted);
    let messages = runner.session().messages();
    let Some(Message::Assistant(answer)) = messages.last() else {
        panic!("the aborted answer is kept: {messages:?}");
    };
    assert_eq!(
        (answer.stop_reason, answer.text().as_str()),
        (StopReason::Aborted, "partial")
    );
}
