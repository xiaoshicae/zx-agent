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
use common::{json_lines, stderr, stdout, zx_agent};

/// An answer that would set the terminal title if a terminal ran it.
const TITLE_ANSWER: &str = "before\u{1b}]0;CANARY\u{7}after";

/// `zx-agent run` without tools against `server`.
fn run(server: &MockServer, dir: &std::path::Path) -> Command {
    let mut command = zx_agent();
    command
        .args(["run", "--model", "mock", "--base-url", &server.base_url])
        .args([
            "--max-retries",
            "0",
            "--api-key-env",
            "ZX_RUN_CLI_TEST_KEY",
            "--tools",
            "",
            "go",
        ])
        .current_dir(dir)
        .env_remove("ZX_RUN_CLI_TEST_KEY")
        .env_remove("ZX_MODEL")
        .env_remove("ZX_BASE_URL")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    command
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
        .args(["run", "--model", "mock", "--base-url", &server.base_url])
        .args(["--max-retries", "0", "--api-key-env", "ZX_RUN_CLI_TEST_KEY", "go"])
        .current_dir(dir.path())
        .env_remove("ZX_RUN_CLI_TEST_KEY")
        .env_remove("ZX_MODEL")
        .env_remove("ZX_BASE_URL")
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

/// Commands run as the same user as the agent. On Linux they must not read the agent's
/// environment, which holds the API key, through /proc.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn commands_cannot_read_the_agent_environment_through_proc() {
    const PROBE: &str = "visible-in-the-agent-environment";
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::bash("c", "cat /proc/$PPID/environ"), Reply::text("done")]).await;
    let output = zx_agent()
        .args(["run", "--model", "mock", "--base-url", &server.base_url])
        .args([
            "--max-retries",
            "0",
            "--api-key-env",
            "ZX_RUN_CLI_TEST_KEY",
            "--tools",
            "bash",
            "go",
        ])
        .current_dir(dir.path())
        .env("ZX_ENVIRON_PROBE", PROBE)
        .env_remove("ZX_RUN_CLI_TEST_KEY")
        .env_remove("ZX_MODEL")
        .env_remove("ZX_BASE_URL")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let result = server.requests()[1].body["messages"].to_string();
    assert!(result.contains("Permission denied"), "{result}");
    assert!(!result.contains(PROBE), "{result}");
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
        .args(["run", "--model", "mock", "--base-url", &server.base_url])
        .args([
            "--max-retries",
            "0",
            "--api-key-env",
            "ZX_RUN_CLI_TEST_KEY",
            "--tools",
            "bash",
            "go",
        ])
        .current_dir(dir.path())
        .env("OTEL_EXPORTER_OTLP_HEADERS", TOKEN)
        .env("OTEL_EXPORTER_OTLP_TRACES_HEADERS", TOKEN)
        .env("OTEL_EXPORTER_OTLP_METRICS_HEADERS", TOKEN)
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .env_remove("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
        .env_remove("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT")
        .env_remove("ZX_RUN_CLI_TEST_KEY")
        .env_remove("ZX_MODEL")
        .env_remove("ZX_BASE_URL")
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
    let output = run(&server, dir.path())
        .args(["--request-field", r#"thinking={"type":"disabled"}"#])
        .args(["--request-field", r#"reasoning_effort="low""#])
        .args(["--session", "audit.jsonl"])
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let body = &server.requests()[0].body;
    assert_eq!(body["thinking"], json!({"type": "disabled"}));
    assert_eq!(body["reasoning_effort"], json!("low"));
    let audit = std::fs::read_to_string(dir.path().join("audit.jsonl")).unwrap();
    assert!(
        // Fields keep the order they were given in: serde_json's `preserve_order` is on for
        // the whole workspace since the ACP SDK enables it.
        audit.contains(r#""extraFields":{"thinking":{"type":"disabled"},"reasoning_effort":"low"}"#),
        "{audit}"
    );
}

#[tokio::test]
async fn bad_request_fields_exit_2_before_any_request() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("never sent")]).await;
    for (field, expected) in [
        (r#"model="other""#, r#"request field "model" cannot be added"#),
        (r#"thinking={"type":"#, "the value is not JSON"),
        ("no-equals-sign", "is not of the form KEY=JSON"),
    ] {
        let output = run(&server, dir.path())
            .args(["--request-field", field, "--session", "audit.jsonl"])
            .output()
            .await
            .unwrap();
        let stderr = stderr(&output);
        assert_eq!(output.status.code(), Some(2), "{field}: {stderr}");
        assert!(stderr.contains(expected), "{field}: {stderr}");
    }
    let output = run(&server, dir.path())
        .args(["--request-field", "seed=1", "--request-field", "seed=2"])
        .output()
        .await
        .unwrap();
    let stderr = stderr(&output);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("--request-field seed is given more than once"),
        "{stderr}"
    );
    assert!(server.requests().is_empty());
    assert!(!dir.path().join("audit.jsonl").exists());
}

#[tokio::test]
async fn the_reasoning_level_reaches_the_model_in_the_chosen_format_and_the_audit() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("ok"), Reply::text("ok"), Reply::text("ok")]).await;
    let cases: [(&[&str], &str, Value); 3] = [
        (
            &["--reasoning", "high"],
            "effort.jsonl",
            json!({"reasoning_effort": "high"}),
        ),
        (
            &["--reasoning", "off", "--reasoning-format", "thinking"],
            "thinking.jsonl",
            json!({"thinking": {"type": "disabled"}}),
        ),
        (&[], "unset.jsonl", json!({})),
    ];
    for (index, (flags, session, expected)) in cases.into_iter().enumerate() {
        let output = run(&server, dir.path())
            .args(flags)
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
        assert_eq!(Value::Object(sent), expected, "{flags:?}");
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
        .args(["run", "--continue", "--model", "mock", "--base-url", &server.base_url])
        .args([
            "--max-retries",
            "0",
            "--api-key-env",
            "ZX_RUN_CLI_TEST_KEY",
            "--tools",
            "",
        ])
        .current_dir(dir)
        .env_remove("ZX_RUN_CLI_TEST_KEY")
        .env_remove("ZX_MODEL")
        .env_remove("ZX_BASE_URL")
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
    for (flags, expected) in [
        (&["--reasoning", "max"][..], "unknown reasoning level"),
        (
            &["--reasoning", "low", "--reasoning-format", "guess"][..],
            "unknown reasoning format",
        ),
        (
            &["--reasoning", "low", "--request-field", r#"reasoning_effort="high""#][..],
            "--request-field reasoning_effort conflicts with --reasoning",
        ),
        (
            &[
                "--reasoning",
                "low",
                "--reasoning-format",
                "thinking",
                "--request-field",
                r#"thinking={"type":"enabled"}"#,
            ][..],
            "--request-field thinking conflicts with --reasoning",
        ),
    ] {
        let output = run(&server, dir.path())
            .args(flags)
            .args(["--session", "audit.jsonl"])
            .output()
            .await
            .unwrap();
        let stderr = stderr(&output);
        assert_eq!(output.status.code(), Some(2), "{flags:?}: {stderr}");
        assert!(stderr.contains(expected), "{flags:?}: {stderr}");
    }
    assert!(server.requests().is_empty());
    assert!(!dir.path().join("audit.jsonl").exists());
}

#[tokio::test]
async fn filling_missing_reasoning_is_off_unless_asked_and_the_audit_records_the_choice() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("ok"), Reply::text("ok")]).await;
    for (flags, session, filled) in [
        (&[][..], "off.jsonl", false),
        (&["--fill-missing-reasoning"][..], "on.jsonl", true),
    ] {
        let output = run(&server, dir.path())
            .args(flags)
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
        assert_eq!(line["schemaVersion"], 1, "{line}");
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

    for flag in ["--tool-timeout-secs", "--idle-timeout-secs"] {
        let output = run(&server, dir.path()).args([flag, "0"]).output().await.unwrap();

        assert_eq!(output.status.code(), Some(2), "{flag}");
        let stderr = stderr(&output);
        assert!(stderr.contains(flag), "{flag}: {stderr}");
    }
    assert!(server.requests().is_empty());
}

/// A cut-off answer, which `run` asks the model to continue (`--continue-on-length` is 1),
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
async fn a_profile_sets_the_token_budget_and_zero_on_the_command_line_disables_it() {
    let dir = tempfile::tempdir().unwrap();
    let profile = dir.path().join("budget.toml");
    std::fs::write(&profile, "[limits]\ntokenBudget = 12\n").unwrap();
    let profile = profile.to_str().unwrap();

    let limited = two_answers().await;
    let output = run(&limited, dir.path())
        .args(["--profile", profile])
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(7), "{}", stderr(&output));
    assert_eq!(limited.requests().len(), 1);

    let unlimited = two_answers().await;
    let output = run(&unlimited, dir.path())
        .args(["--profile", profile, "--token-budget", "0"])
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(unlimited.requests().len(), 2);
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
async fn a_run_exports_its_trace_and_metrics_only_when_an_endpoint_is_configured() {
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
    assert!(
        stderr.contains("[otel] exporting traces and metrics over OTLP/HTTP"),
        "{stderr}"
    );
    {
        let requests = received.lock().unwrap();
        // The process exports what remains before it exits: the spans, then the metrics.
        let mut paths: Vec<&str> = requests.iter().map(|request| request.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(paths, ["/v1/metrics", "/v1/traces"]);
        for request in requests.iter() {
            assert_eq!(request.method, "POST");
            assert_eq!(request.header("content-type"), Some("application/x-protobuf"));
            // Protobuf keeps strings as they are: the service name, and an attribute of the
            // run span or the name of a metric.
            assert!(contains(&request.body, "zx-agent"));
            let marker = if request.path == "/v1/traces" {
                "zx.run.id"
            } else {
                "zx.runs"
            };
            assert!(contains(&request.body, marker), "{}", request.path);
            assert!(!contains(&request.body, "the answer"), "no content leaves the process");
        }
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
    assert_eq!(received.lock().unwrap().len(), 2, "nothing more was sent");
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
