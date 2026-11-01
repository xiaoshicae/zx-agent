//! The OpenAI-compatible provider over real HTTP against a local fake server: SSE parsing,
//! tool results sent back, retries, timeouts, and cancellation. No network or API key needed.

mod common;

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio_util::sync::CancellationToken;

use common::mock_server::{MockServer, Reply};
use common::{CancelOnFirstDelta, Collect, RunnerSetup, config, fast_retry};
use zx_agent::agent::{AgentEvent, Event, EventSink};
use zx_agent::ai::openai::{OpenAiCompatible, OpenAiConfig};
use zx_agent::ai::retry::RetryPolicy;
use zx_agent::ai::{Message, PREAMBLE, ProviderError, StopReason, SystemPrompt};
use zx_agent::runner::RunOutcome;
use zx_agent::tools::BashTool;

/// A runner whose provider talks to `server`, with a bash tool and a one-line system prompt.
fn setup(server: &MockServer, idle_timeout: Duration) -> RunnerSetup {
    setup_with_provider(
        server,
        OpenAiConfig {
            idle_timeout,
            ..OpenAiConfig::default()
        },
    )
}

fn setup_with_provider(server: &MockServer, mut settings: OpenAiConfig) -> RunnerSetup {
    settings.base_url = server.base_url.clone();
    settings.api_key = Some("test-key".into());
    settings.use_system_proxy = false;
    let provider = OpenAiCompatible::new(settings).unwrap();
    let mut config = config(Arc::new(provider), vec![Arc::new(BashTool::default())]);
    config.spec.model = "mock-model".into();
    config.spec.system_prompt = SystemPrompt::default().with_section(PREAMBLE, "You are a test.");
    config.spec.cwd = std::env::temp_dir();
    config.spec.max_tokens = Some(256);
    RunnerSetup::new(config)
}

fn raw_error(status: u16, headers: &str, body: &str) -> Reply {
    Reply::Raw(format!(
        "HTTP/1.1 {status} Error\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{headers}connection: close\r\n\r\n{body}",
        body.len()
    ))
}

fn provider_error(runner: &zx_agent::runner::Runner) -> ProviderError {
    match runner.session().messages().last() {
        Some(Message::Assistant(message)) => message.provider_error.clone().expect("structured provider error"),
        other => panic!("expected a failed assistant message: {other:?}"),
    }
}

#[tokio::test]
async fn response_budget_failure_is_not_retried() {
    let server = MockServer::start(vec![Reply::text(&"x".repeat(256))]).await;
    let (mut runner, _) = RunnerSetup {
        retry: fast_retry(3),
        ..setup_with_provider(
            &server,
            OpenAiConfig {
                max_response_bytes: 64,
                ..OpenAiConfig::default()
            },
        )
    }
    .build();

    let report = runner.run("check", &CancellationToken::new()).await;

    assert_eq!((report.outcome, report.retries), (RunOutcome::Error, 0));
    assert_eq!(provider_error(&runner), ProviderError::ResourceLimit);
    assert!(report.error.unwrap().contains("response exceeds the 64 byte limit"));
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn sse_event_budget_is_enforced() {
    let server = MockServer::start(vec![Reply::text("small text in an oversized event")]).await;
    let (mut runner, _) = setup_with_provider(
        &server,
        OpenAiConfig {
            max_sse_event_bytes: 32,
            ..OpenAiConfig::default()
        },
    )
    .build();

    let report = runner.run("check", &CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Error);
    assert_eq!(provider_error(&runner), ProviderError::ResourceLimit);
    assert!(report.error.unwrap().contains("SSE event exceeds the 32 byte limit"));
}

#[tokio::test]
async fn http_status_prevents_retries_from_diagnostic_words() {
    let server = MockServer::start(vec![Reply::Status(400, r#"{"error":{"message":"connection id 503"}}"#)]).await;
    let (mut runner, _) = RunnerSetup {
        retry: fast_retry(3),
        ..setup(&server, Duration::from_secs(10))
    }
    .build();

    let report = runner.run("check", &CancellationToken::new()).await;

    assert_eq!((report.outcome, report.retries), (RunOutcome::Error, 0));
    assert_eq!(
        provider_error(&runner),
        ProviderError::Http {
            status: 400,
            code: None,
            retry_after_ms: None
        }
    );
}

#[tokio::test]
async fn quota_errors_are_not_retried() {
    let server = MockServer::start(vec![Reply::Status(
        429,
        r#"{"error":{"message":"rate limit","code":"insufficient_quota"}}"#,
    )])
    .await;
    let (mut runner, _) = RunnerSetup {
        retry: fast_retry(3),
        ..setup(&server, Duration::from_secs(10))
    }
    .build();

    let report = runner.run("check", &CancellationToken::new()).await;

    assert_eq!((report.outcome, report.retries), (RunOutcome::Error, 0));
    assert_eq!(
        provider_error(&runner),
        ProviderError::Http {
            status: 429,
            code: Some("insufficient_quota".into()),
            retry_after_ms: None,
        }
    );
}

#[tokio::test]
async fn numeric_retry_after_is_used_by_the_runner() {
    let server = MockServer::start(vec![raw_error(
        429,
        "retry-after: 7\r\n",
        r#"{"error":{"code":"rate_limit_exceeded","message":"slow down"}}"#,
    )])
    .await;
    let cancel = CancellationToken::new();
    let observed = Arc::new(Collect::default());
    let captured = observed.clone();
    let trigger = cancel.clone();
    let mut runner = RunnerSetup {
        retry: RetryPolicy::default(),
        ..setup(&server, Duration::from_secs(10))
    }
    .build_with(Arc::new(move |event: &Event| {
        if matches!(event.event, AgentEvent::RetryScheduled { .. }) {
            trigger.cancel();
        }
        captured.emit(event);
    }));

    let report = runner.run("check", &cancel).await;

    assert_eq!(report.outcome, RunOutcome::Aborted);
    assert!(
        observed
            .events()
            .iter()
            .any(|event| matches!(event, AgentEvent::RetryScheduled { delay_ms: 7000, .. }))
    );
    assert!(observed.events().iter().any(|event| matches!(
        event, AgentEvent::MessageEnd { message: Message::Assistant(message), .. }
            if message.provider_error == Some(ProviderError::Http {
                status: 429, code: Some("rate_limit_exceeded".into()), retry_after_ms: Some(7000),
            })
    )));
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn large_http_errors_keep_bounded_diagnostics() {
    let server = MockServer::start(vec![raw_error(400, "", &"x".repeat(128 * 1024))]).await;
    let (mut runner, _) = setup(&server, Duration::from_secs(10)).build();

    let report = runner.run("check", &CancellationToken::new()).await;

    let error = report.error.unwrap();
    assert!(error.starts_with("HTTP 400: "));
    assert!(error.len() <= 2013, "unbounded diagnostic length: {}", error.len());
}

#[tokio::test]
async fn invalid_stream_json_is_a_non_retryable_protocol_error() {
    let response = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n\
        data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\ndata: {broken}\n\n";
    let server = MockServer::start(vec![Reply::Raw(response.into())]).await;
    let (mut runner, _) = RunnerSetup {
        retry: fast_retry(3),
        ..setup(&server, Duration::from_secs(10))
    }
    .build();

    let report = runner.run("check", &CancellationToken::new()).await;

    assert_eq!((report.outcome, report.retries), (RunOutcome::Error, 0));
    assert_eq!(report.final_text, "partial");
    assert_eq!(provider_error(&runner), ProviderError::Protocol);
}

#[tokio::test]
async fn an_unknown_finish_reason_is_a_provider_error_not_a_completed_answer() {
    let server = MockServer::start(vec![Reply::Sse(vec![json!({"choices":[{
        "index": 0, "delta": {"content": "partial"}, "finish_reason": "server_interrupted"
    }]})])])
    .await;
    let (mut runner, _) = RunnerSetup {
        retry: fast_retry(3),
        ..setup(&server, Duration::from_secs(10))
    }
    .build();

    let report = runner.run("check", &CancellationToken::new()).await;

    assert_eq!((report.outcome, report.retries), (RunOutcome::Error, 0), "{report:?}");
    let error = report.error.unwrap_or_default();
    assert!(error.contains("server_interrupted"), "{error}");
    assert_eq!(
        provider_error(&runner),
        ProviderError::Provider {
            code: Some("server_interrupted".into())
        }
    );
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
    let (mut runner, _) = setup(&server, Duration::from_secs(10)).build();

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
    let (mut runner, _) = RunnerSetup {
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
    let (mut runner, _) = setup(&server, Duration::from_secs(1)).build();

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
    let mut runner = setup(&server, Duration::from_secs(10)).build_with(Arc::new(CancelOnFirstDelta(cancel.clone())));

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
