//! `run` output contracts added after P0, against the local fake HTTP service. The original
//! run tests stay byte-identical in tests/cli.rs.
mod common;

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::process::Command;

use common::http::{Request, read_request};
use common::mock_server::{MockServer, Reply};
use common::{AgentHome, fake_model_config, json_lines, stderr, stdout, write_config, zx_agent};

/// An answer that would set the terminal title if a terminal ran it.
const TITLE_ANSWER: &str = "before\u{1b}]0;CANARY\u{7}after";

/// `zx-agent run` without tools against `server`.
fn run(server: &MockServer, dir: &std::path::Path) -> Command {
    run_with(server, dir, "", "")
}

/// `zx-agent run` without tools against `server`, with a config.toml in a zx-agent home of
/// `dir` that has `top` before its `[provider]` table and `rest` after the test's provider
/// keys.
fn run_with(server: &MockServer, dir: &std::path::Path, top: &str, rest: &str) -> Command {
    let mut command = zx_agent();
    command
        .args(["run", "--model", "mock", "--tools", "", "go"])
        .current_dir(dir)
        .env("ZX_AGENT_HOME", home(dir, top, rest))
        .env("ZX_BASE_URL", &server.base_url)
        .env_remove("ZX_RUN_CLI_TEST_KEY")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    command
}

/// A zx-agent home in `dir`: a config.toml for the fake model, with `top` and `rest`.
fn home(dir: &std::path::Path, top: &str, rest: &str) -> std::path::PathBuf {
    let home = dir.join(".home");
    write_config(&home, &fake_model_config("ZX_RUN_CLI_TEST_KEY", top, rest));
    home
}

#[tokio::test]
async fn an_answer_written_to_a_pipe_keeps_its_exact_text() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text(TITLE_ANSWER)]).await;
    let output = run(&server, dir.path()).output().await.unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(stdout(&output), format!("{TITLE_ANSWER}\n"));
}

#[cfg(unix)]
#[tokio::test]
async fn an_answer_written_to_a_terminal_cannot_drive_it() {
    use tokio::io::AsyncReadExt;

    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text(TITLE_ANSWER)]).await;
    let (mut terminal, slave) = common::pty();
    let mut command = run(&server, dir.path());
    command.stdout(Stdio::from(slave)).stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    // The child holds the only other copy of the terminal, so reading ends when it exits.
    drop(command);
    let reader = tokio::spawn(async move {
        let mut seen = Vec::new();
        let mut chunk = [0u8; 4096];
        while let Ok(read) = terminal.read(&mut chunk).await {
            if read == 0 {
                break;
            }
            seen.extend_from_slice(&chunk[..read]);
        }
        seen
    });
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.code(), Some(0));
    let seen = tokio::time::timeout(Duration::from_secs(10), reader)
        .await
        .unwrap()
        .unwrap();
    let text = String::from_utf8_lossy(&seen);
    assert!(!text.contains('\u{1b}'), "{text:?}");
    assert!(text.contains("before\\u{1b}]0;CANARY\\u{7}after"), "{text:?}");
}

#[tokio::test]
async fn progress_and_report_lines_escape_terminal_control_characters() {
    let dir = tempfile::tempdir().unwrap();
    // The model names a tool that does not exist, then the server fails with a body that
    // tries to clear the screen. Both texts reach stderr.
    let server = MockServer::start(vec![
        Reply::Sse(vec![json!({"choices": [{"index": 0, "delta": {"tool_calls": [{
            "index": 0, "id": "c1", "type": "function",
            "function": {"name": "x\u{1b}[2J", "arguments": "{}"}
        }]}, "finish_reason": "tool_calls"}]})]),
        Reply::Status(400, "bad\u{1b}[2J"),
    ])
    .await;
    let output = zx_agent()
        .args(["run", "--model", "mock", "go"])
        .current_dir(dir.path())
        .env("ZX_AGENT_HOME", home(dir.path(), "", ""))
        .env("ZX_BASE_URL", &server.base_url)
        .env_remove("ZX_RUN_CLI_TEST_KEY")
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    let stderr = stderr(&output);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert_eq!(server.requests().len(), 2, "{stderr}");
    assert!(!stderr.contains('\u{1b}'), "{stderr:?}");
    assert!(stderr.contains("x\\u{1b}[2J"), "{stderr}");
    assert!(stderr.contains("bad\\u{1b}[2J"), "{stderr}");
}

#[tokio::test]
async fn a_second_run_cannot_write_a_session_that_another_run_is_writing() {
    let dir = tempfile::tempdir().unwrap();
    let session = dir.path().join("session.jsonl");
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let chunks = vec![json!({"choices":[{"index":0,"delta":{"content":"first"},"finish_reason":null}]})];
    let server = MockServer::start(vec![
        Reply::Gate {
            chunks,
            release: release.clone(),
        },
        Reply::text("second"),
    ])
    .await;
    let mut first = run(&server, dir.path());
    first
        .args(["--session", session.to_str().unwrap()])
        .stdout(Stdio::piped());
    let first = first.spawn().unwrap();
    // The first run has written its input and is waiting for the model.
    server.wait_for_requests(1).await;
    let before = std::fs::read(&session).unwrap();

    let second = run(&server, dir.path())
        .args(["--session", session.to_str().unwrap()])
        .output()
        .await
        .unwrap();

    let stderr = stderr(&second);
    assert_eq!(second.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("is in use"), "{stderr}");
    assert_eq!(
        std::fs::read(&session).unwrap(),
        before,
        "the second run wrote to the session"
    );
    release.notify_one();
    let first = tokio::time::timeout(Duration::from_secs(10), first.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.status.code(), Some(0));
    assert_eq!(server.requests().len(), 1, "the second run sent a request");
}

#[cfg(unix)]
#[tokio::test]
async fn commands_cannot_see_the_headers_for_a_trace_collector() {
    const TOKEN: &str = "Authorization=Bearer collector-secret";
    let dir = tempfile::tempdir().unwrap();
    let command = "echo \"headers=[$OTEL_EXPORTER_OTLP_HEADERS] traces=[$OTEL_EXPORTER_OTLP_TRACES_HEADERS] \
                   metrics=[$OTEL_EXPORTER_OTLP_METRICS_HEADERS]\"";
    let server = MockServer::start(vec![Reply::bash("c", command), Reply::text("done")]).await;
    let output = zx_agent()
        .args(["run", "--model", "mock", "--tools", "bash", "go"])
        .current_dir(dir.path())
        .env("ZX_AGENT_HOME", home(dir.path(), "", ""))
        .env("ZX_BASE_URL", &server.base_url)
        .env("OTEL_EXPORTER_OTLP_HEADERS", TOKEN)
        .env("OTEL_EXPORTER_OTLP_TRACES_HEADERS", TOKEN)
        .env("OTEL_EXPORTER_OTLP_METRICS_HEADERS", TOKEN)
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .env_remove("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
        .env_remove("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT")
        .env_remove("ZX_RUN_CLI_TEST_KEY")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let result = server.requests()[1].body["messages"].to_string();
    assert!(result.contains("headers=[] traces=[] metrics=[]"), "{result}");
    assert!(!result.contains("collector-secret"), "{result}");
}

#[tokio::test]
async fn request_fields_reach_the_model_and_the_audit() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("ok")]).await;
    let fields = r#"requestFields = { thinking = { type = "disabled" }, reasoning_effort = "low" }"#;
    let output = run_with(&server, dir.path(), "", fields)
        .args(["--session", "audit.jsonl"])
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let body = &server.requests()[0].body;
    assert_eq!(body["thinking"], json!({"type": "disabled"}));
    assert_eq!(body["reasoning_effort"], json!("low"));
    let audit = std::fs::read_to_string(dir.path().join("audit.jsonl")).unwrap();
    let config = json_lines(&audit)
        .into_iter()
        .find(|record| record["type"] == "request_config")
        .unwrap();
    // Compared as maps: the order of a TOML table's keys is the toml crate's.
    assert_eq!(
        config["config"]["providerOptions"]["extraFields"],
        json!({"thinking": {"type": "disabled"}, "reasoning_effort": "low"}),
        "{audit}"
    );
}

#[tokio::test]
async fn bad_request_fields_exit_2_before_any_request() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("never sent")]).await;
    for (fields, expected) in [
        (
            r#"requestFields = { model = "other" }"#,
            r#"request field "model" cannot be added"#,
        ),
        ("requestFields = { seed = 1, seed = 2 }", "duplicate key"),
        (r#"requestFields = "seed=1""#, "invalid type"),
    ] {
        let output = run_with(&server, dir.path(), "", fields)
            .args(["--session", "audit.jsonl"])
            .output()
            .await
            .unwrap();
        let stderr = stderr(&output);
        assert_eq!(output.status.code(), Some(2), "{fields}: {stderr}");
        assert!(stderr.contains(expected), "{fields}: {stderr}");
        assert!(stderr.contains("config.toml"), "{fields}: {stderr}");
    }
    assert!(server.requests().is_empty());
    assert!(!dir.path().join("audit.jsonl").exists());
}

#[tokio::test]
async fn the_reasoning_level_reaches_the_model_in_the_chosen_format_and_the_audit() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("ok"), Reply::text("ok"), Reply::text("ok")]).await;
    let cases: [(&str, &str, &str, Value); 3] = [
        (
            "reasoning = 'high'",
            "",
            "effort.jsonl",
            json!({"reasoning_effort": "high"}),
        ),
        (
            "reasoning = 'off'",
            "reasoningFormat = 'thinking'",
            "thinking.jsonl",
            json!({"thinking": {"type": "disabled"}}),
        ),
        ("", "", "unset.jsonl", json!({})),
    ];
    for (index, (top, provider, session, expected)) in cases.into_iter().enumerate() {
        let output = run_with(&server, dir.path(), top, provider)
            .args(["--session", session])
            .output()
            .await
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        let body = &server.requests()[index].body;
        let mut sent = serde_json::Map::new();
        for field in ["reasoning_effort", "thinking", "reasoning"] {
            if let Some(value) = body.get(field) {
                sent.insert(field.into(), value.clone());
            }
        }
        assert_eq!(Value::Object(sent), expected, "{top} {provider}");
    }
    let effort = std::fs::read_to_string(dir.path().join("effort.jsonl")).unwrap();
    assert!(effort.contains(r#""reasoning":"high""#), "{effort}");
    let thinking = std::fs::read_to_string(dir.path().join("thinking.jsonl")).unwrap();
    assert!(thinking.contains(r#""reasoning":"off""#), "{thinking}");
    assert!(thinking.contains(r#""reasoningFormat":"thinking""#), "{thinking}");
    let unset = std::fs::read_to_string(dir.path().join("unset.jsonl")).unwrap();
    assert!(!unset.contains("reasoning"), "{unset}");
}

/// `zx-agent run --continue` against `server`, without a prompt.
fn run_continue(server: &MockServer, dir: &std::path::Path) -> Command {
    let mut command = zx_agent();
    command
        .args(["run", "--continue", "--model", "mock", "--tools", ""])
        .current_dir(dir)
        .env("ZX_AGENT_HOME", home(dir, "", ""))
        .env("ZX_BASE_URL", &server.base_url)
        .env_remove("ZX_RUN_CLI_TEST_KEY")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    command
}

#[tokio::test]
async fn continue_answers_again_after_a_failed_run_without_repeating_the_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![
        Reply::Status(400, r#"{"error":{"message":"bad request"}}"#),
        Reply::text("second try"),
        Reply::text("must not be requested"),
    ])
    .await;
    let failed = run(&server, dir.path())
        .args(["--session", "s.jsonl"])
        .output()
        .await
        .unwrap();
    assert_eq!(failed.status.code(), Some(1));

    let continued = run_continue(&server, dir.path())
        .args(["--session", "s.jsonl"])
        .output()
        .await
        .unwrap();

    assert_eq!(continued.status.code(), Some(0), "{}", stderr(&continued));
    assert_eq!(stdout(&continued), "second try\n");
    let requests = server.requests();
    let users: Vec<&Value> = requests[1].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "user")
        .collect();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0]["content"], "go");

    // Ending with a finished answer, there is nothing more to continue.
    let finished = run_continue(&server, dir.path())
        .args(["--session", "s.jsonl"])
        .output()
        .await
        .unwrap();
    assert_eq!(finished.status.code(), Some(1));
    assert!(stderr(&finished).contains("ends with a finished answer"));
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn continue_needs_an_existing_session_and_no_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("never sent")]).await;
    for (extra, expected) in [
        (&[][..], "--session"),
        (&["--session", "missing.jsonl"][..], "missing.jsonl does not exist"),
        (&["--session", "s.jsonl", "a prompt"][..], "cannot be used with"),
    ] {
        let output = run_continue(&server, dir.path()).args(extra).output().await.unwrap();
        let stderr = stderr(&output);
        assert_eq!(output.status.code(), Some(2), "{extra:?}: {stderr}");
        assert!(stderr.contains(expected), "{extra:?}: {stderr}");
    }
    assert!(server.requests().is_empty());
    assert!(!dir.path().join("missing.jsonl").exists());
}

#[tokio::test]
async fn bad_reasoning_settings_exit_2_before_any_request() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("never sent")]).await;
    for (top, provider, expected) in [
        ("reasoning = 'max'", "", "unknown reasoning level"),
        (
            "reasoning = 'low'",
            "reasoningFormat = 'guess'",
            "unknown reasoning format",
        ),
        (
            "reasoning = 'low'",
            "requestFields = { reasoning_effort = 'high' }",
            "provider.requestFields.reasoning_effort conflicts with reasoning",
        ),
        (
            "reasoning = 'low'",
            "reasoningFormat = 'thinking'\nrequestFields = { thinking = { type = 'enabled' } }",
            "provider.requestFields.thinking conflicts with reasoning",
        ),
    ] {
        let output = run_with(&server, dir.path(), top, provider)
            .args(["--session", "audit.jsonl"])
            .output()
            .await
            .unwrap();
        let stderr = stderr(&output);
        assert_eq!(output.status.code(), Some(2), "{top} {provider}: {stderr}");
        assert!(stderr.contains(expected), "{top} {provider}: {stderr}");
    }
    assert!(server.requests().is_empty());
    assert!(!dir.path().join("audit.jsonl").exists());
}

#[tokio::test]
async fn filling_missing_reasoning_is_off_unless_asked_and_the_audit_records_the_choice() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("ok"), Reply::text("ok")]).await;
    for (provider, session, filled) in [
        ("", "off.jsonl", false),
        ("fillMissingReasoning = true", "on.jsonl", true),
    ] {
        let output = run_with(&server, dir.path(), "", provider)
            .args(["--session", session])
            .output()
            .await
            .unwrap();
        let stderr = stderr(&output);
        assert_eq!(output.status.code(), Some(0), "{session}: {stderr}");
        let audit = std::fs::read_to_string(dir.path().join(session)).unwrap();
        assert!(audit.contains(r#""version":2"#), "{audit}");
        assert_eq!(audit.contains(r#""fillMissingReasoning":true"#), filled, "{audit}");
    }
}

#[tokio::test]
async fn jsonl_events_carry_the_envelope_of_their_run() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("ok")]).await;

    // With the streaming deltas, so that every event of the run is written.
    let output = run(&server, dir.path())
        .args(["--output", "jsonl", "--deltas"])
        .output()
        .await
        .unwrap();

    assert_eq!(output.status.code(), Some(0));
    let lines: Vec<Value> = json_lines(&stdout(&output));
    for (index, line) in lines.iter().enumerate() {
        // 2 since `costUsd` became `totalCost`.
        assert_eq!(line["schemaVersion"], 2, "{line}");
        assert_eq!(line["seq"], index + 1, "{line}");
        assert_eq!(line["runId"], lines[0]["runId"], "{line}");
        assert!(line["sessionId"].is_string() && line["atMs"].is_u64(), "{line}");
    }
    assert_eq!(lines[0]["type"], "run_start");
    // The run's first attempt follows it.
    assert_eq!(
        (lines[1]["type"].clone(), lines[1]["attempt"].clone()),
        (json!("agent_start"), json!(1))
    );
    let end = lines.last().unwrap();
    assert_eq!(
        (end["type"].clone(), end["report"]["finalText"].clone()),
        (json!("run_end"), json!("ok"))
    );
}

#[tokio::test]
async fn a_zero_timeout_is_rejected_before_any_request() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("never sent")]).await;

    for (top, provider, key) in [
        ("toolTimeoutSecs = 0", "", "toolTimeoutSecs"),
        ("", "idleTimeoutSecs = 0", "provider.idleTimeoutSecs"),
    ] {
        let output = run_with(&server, dir.path(), top, provider).output().await.unwrap();

        assert_eq!(output.status.code(), Some(2), "{key}");
        let stderr = stderr(&output);
        assert!(stderr.contains(&format!("{key} must be at least 1")), "{key}: {stderr}");
    }
    assert!(server.requests().is_empty());
}

/// A cut-off answer, which `run` asks the model to continue (the unattended agent's
/// `continueOnLength` is 1),
/// then the rest. Each answer of the fake service reports 12 tokens: 10 in, 2 out.
async fn two_answers() -> MockServer {
    MockServer::start(vec![Reply::truncated("part one, "), Reply::text("part two")]).await
}

#[tokio::test]
async fn the_token_budget_ends_a_run_with_exit_code_7_before_the_next_request() {
    let dir = tempfile::tempdir().unwrap();
    let server = two_answers().await;

    let output = run(&server, dir.path())
        .args(["--token-budget", "12"])
        .output()
        .await
        .unwrap();

    let stderr = stderr(&output);
    assert_eq!(output.status.code(), Some(7), "{stderr}");
    assert!(stderr.contains("outcome=TokenBudget"), "{stderr}");
    assert!(
        stderr.contains("token budget reached (12 tokens used, token_budget = 12)"),
        "{stderr}"
    );
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn the_last_request_may_take_a_run_over_its_token_budget() {
    let dir = tempfile::tempdir().unwrap();
    let server = two_answers().await;

    let output = run(&server, dir.path())
        .args(["--token-budget", "13"])
        .output()
        .await
        .unwrap();

    let stderr = stderr(&output);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert_eq!(stdout(&output), "part one, part two\n");
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn the_agent_sets_the_token_budget_and_zero_on_the_command_line_disables_it() {
    let dir = tempfile::tempdir().unwrap();
    let home = AgentHome::agent("unattended", "[limits]\ntokenBudget = 12\n");

    let limited = two_answers().await;
    let output = run(&limited, dir.path())
        .env("ZX_AGENT_HOME", home.path())
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(7), "{}", stderr(&output));
    assert_eq!(limited.requests().len(), 1);

    let unlimited = two_answers().await;
    let output = run(&unlimited, dir.path())
        .args(["--token-budget", "0"])
        .env("ZX_AGENT_HOME", home.path())
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(unlimited.requests().len(), 2);
}

/// A config.toml whose model `mock` costs 0.01 per answer of the fake service (10 input
/// tokens at 1000 per million; output free). The unit is whatever the prices use, such as CNY.
fn priced_config() -> AgentHome {
    AgentHome::new("[[models]]\nid = \"mock\"\ncost = { input = 1000.0, output = 0.0 }\n")
}

#[tokio::test]
async fn the_cost_budget_ends_a_run_with_exit_code_8_before_the_next_request() {
    let dir = tempfile::tempdir().unwrap();
    let server = two_answers().await;
    let home = priced_config();

    let output = run(&server, dir.path())
        .args(["--cost-budget", "0.01"])
        .env("ZX_AGENT_HOME", home.path())
        .output()
        .await
        .unwrap();

    let stderr = stderr(&output);
    assert_eq!(output.status.code(), Some(8), "{stderr}");
    assert!(stderr.contains("outcome=CostBudget"), "{stderr}");
    assert!(stderr.contains("cost=0.0100"), "{stderr}");
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn a_cost_budget_without_prices_for_the_model_is_a_configuration_error() {
    let dir = tempfile::tempdir().unwrap();
    let server = two_answers().await;

    let output = run(&server, dir.path())
        .args(["--cost-budget", "0.01"])
        .output()
        .await
        .unwrap();

    let stderr = stderr(&output);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("[[models]]"), "{stderr}");
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn a_negative_cost_budget_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let server = two_answers().await;

    let output = run(&server, dir.path())
        .args(["--cost-budget=-1"])
        .output()
        .await
        .unwrap();

    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(server.requests().is_empty());
}

/// `run` with bash asking except for `ls`, against a model that runs `echo hi` and then
/// answers: nobody can approve the command, so it is refused.
async fn run_with_a_denied_command(extra: &[&str]) -> std::process::Output {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::bash("c", "echo hi"), Reply::text("done")]).await;
    let home = AgentHome::agent("unattended", "[permissions]\nask = [\"bash\"]\nallow = [\"bash(ls)\"]");
    zx_agent()
        .args(["run", "--model", "mock", "--tools", "bash"])
        .env("ZX_BASE_URL", &server.base_url)
        .env("ZX_AGENT_HOME", home.path())
        .args(extra)
        .arg("go")
        .current_dir(dir.path())
        .env_remove("ZX_RUN_CLI_TEST_KEY")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_completed_run_with_a_denied_call_exits_0_and_reports_the_count() {
    let output = run_with_a_denied_command(&[]).await;

    let stderr = stderr(&output);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(stderr.contains("denied_calls=1"), "{stderr}");
}

#[tokio::test]
async fn fail_on_denied_exits_9_when_a_completed_run_had_a_denied_call() {
    let output = run_with_a_denied_command(&["--fail-on-denied"]).await;

    let stderr = stderr(&output);
    assert_eq!(output.status.code(), Some(9), "{stderr}");
    assert!(stderr.contains("outcome=Completed"), "{stderr}");
}

#[tokio::test]
async fn fail_on_denied_keeps_exit_code_0_when_nothing_was_denied() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("done")]).await;

    let output = run(&server, dir.path()).arg("--fail-on-denied").output().await.unwrap();

    let stderr = stderr(&output);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(!stderr.contains("denied_calls"), "{stderr}");
}

/// A trace collector that records each OTLP/HTTP request and answers it as accepted.
async fn collector() -> (String, Arc<Mutex<Vec<Request>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let received = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&received);
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            if let Some(request) = read_request(&mut stream).await {
                log.lock().unwrap().push(request);
            }
            let accepted = "HTTP/1.1 200 OK\r\ncontent-type: application/x-protobuf\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
            stream.write_all(accepted.as_bytes()).await.ok();
        }
    });
    (endpoint, received)
}

fn contains(body: &[u8], text: &str) -> bool {
    contains_bytes(body, text.as_bytes())
}

fn contains_bytes(body: &[u8], bytes: &[u8]) -> bool {
    body.windows(bytes.len()).any(|window| window == bytes)
}

/// The bytes that a hex ID is sent as.
fn hex(id: &str) -> Vec<u8> {
    (0..id.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&id[at..at + 2], 16).unwrap())
        .collect()
}

#[tokio::test]
async fn a_run_exports_its_trace_only_when_an_endpoint_is_configured() {
    let dir = tempfile::tempdir().unwrap();
    let (endpoint, received) = collector().await;

    let server = MockServer::start(vec![Reply::text("the answer")]).await;
    let output = run(&server, dir.path())
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", &endpoint)
        .env_remove("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
        .env_remove("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT")
        .env_remove("TRACEPARENT")
        .output()
        .await
        .unwrap();
    let stderr = stderr(&output);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(stderr.contains("[otel] exporting traces over OTLP/HTTP"), "{stderr}");
    {
        let requests = received.lock().unwrap();
        // The process exports the spans before it exits, and no metrics, even under the
        // endpoint variable that would name both.
        let paths: Vec<&str> = requests.iter().map(|request| request.path.as_str()).collect();
        assert_eq!(paths, ["/v1/traces"]);
        let request = &requests[0];
        assert_eq!(request.method, "POST");
        assert_eq!(request.header("content-type"), Some("application/x-protobuf"));
        // Protobuf keeps strings as they are: the service name, and an attribute of the run span.
        assert!(contains(&request.body, "zx-agent"));
        assert!(contains(&request.body, "zx.run.id"));
        assert!(!contains(&request.body, "the answer"), "no content leaves the process");
    }

    let server = MockServer::start(vec![Reply::text("the answer")]).await;
    let output = run(&server, dir.path())
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .env_remove("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
        .env_remove("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT")
        .env("OTEL_SERVICE_NAME", "not-a-reason-to-export")
        .output()
        .await
        .unwrap();
    let stderr = common::stderr(&output);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(!stderr.contains("[otel]"), "{stderr}");
    assert_eq!(received.lock().unwrap().len(), 1, "nothing more was sent");
}

#[tokio::test]
async fn a_run_continues_the_trace_that_traceparent_names() {
    const TRACE_ID: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
    const PARENT_SPAN_ID: &str = "00f067aa0ba902b7";
    let dir = tempfile::tempdir().unwrap();
    let (endpoint, received) = collector().await;
    let traces = format!("{endpoint}/v1/traces");

    let server = MockServer::start(vec![Reply::text("the answer")]).await;
    let output = run(&server, dir.path())
        .env("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", &traces)
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .env_remove("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT")
        .env("TRACEPARENT", format!("00-{TRACE_ID}-{PARENT_SPAN_ID}-01"))
        .output()
        .await
        .unwrap();
    let stderr = stderr(&output);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(stderr.contains("[otel] exporting traces over OTLP/HTTP"), "{stderr}");
    assert!(
        stderr.contains("[otel] continuing trace 4bf92f3577b34da6a3ce929d0e0e4736"),
        "{stderr}"
    );
    {
        let requests = received.lock().unwrap();
        assert_eq!(requests.len(), 1, "only traces were configured");
        assert_eq!(requests[0].path, "/v1/traces");
        // Protobuf keeps the IDs as bytes: the caller's trace, and its span as the run's parent.
        assert!(contains_bytes(&requests[0].body, &hex(TRACE_ID)));
        assert!(contains_bytes(&requests[0].body, &hex(PARENT_SPAN_ID)));
    }

    // A value that is not a traceparent is reported; the run goes on in a trace of its own.
    let server = MockServer::start(vec![Reply::text("the answer")]).await;
    let output = run(&server, dir.path())
        .env("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", &traces)
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .env_remove("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT")
        .env("TRACEPARENT", "not-a-traceparent")
        .output()
        .await
        .unwrap();
    let stderr = common::stderr(&output);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(
        stderr.contains(
            "[otel] TRACEPARENT is not a W3C traceparent: \"not-a-traceparent\"; the run starts a trace of its own"
        ),
        "{stderr}"
    );
    let requests = received.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(!contains_bytes(&requests[1].body, &hex(TRACE_ID)));
}

#[tokio::test]
async fn a_closed_stdout_leaves_the_exit_code_to_the_outcome() {
    let dir = tempfile::tempdir().unwrap();
    for mode in ["text", "jsonl"] {
        let server = MockServer::start(vec![Reply::text("the answer")]).await;
        let mut child = run(&server, dir.path())
            .args(["--output", mode])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // Whoever was to read the output is gone before the run starts.
        drop(child.stdout.take());

        let output = child.wait_with_output().await.unwrap();

        let stderr = stderr(&output);
        assert_eq!(output.status.code(), Some(0), "{mode}: {stderr}");
        assert_eq!(server.requests().len(), 1, "{mode}");
    }
}
