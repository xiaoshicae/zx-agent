//! `zx-agent run` as a separate process against the local fake server: exit codes, output
//! modes and their escaping, the session and `--continue`, provider and reasoning settings,
//! budgets, denied calls, trace export, and the wiring that only main.rs does. No network or
//! API key needed.
mod common;
#[cfg(unix)]
#[path = "common/pty.rs"]
mod pty;

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::process::Command;

use common::zx_agent_in;
use zx_testing::http::{Request, read_request, silent_listener};
use zx_testing::mock_server::{MockServer, Reply};
use zx_testing::program::{
    AgentHome, Home, agent_text, ended, ended_output, fake_model_config, write_agent_file, write_config,
};
use zx_testing::{json_lines, stderr, stdout};

/// The variable the program reads the API key from, and the fake key it holds.
const KEY_VAR: &str = "ZX_RUN_CLI_TEST_KEY";
const KEY: &str = "sk-run-cli-test-key";

/// An answer that would set the terminal title if a terminal ran it.
const TITLE_ANSWER: &str = "before\u{1b}]0;CANARY\u{7}after";

/// A zx-agent home in `dir`: a config.toml for the fake model, with `top` before its
/// `[provider]` table and `rest` after the test's provider keys.
fn home(dir: &Path, top: &str, rest: &str) -> PathBuf {
    let home = dir.join(".home");
    write_config(&home, &fake_model_config(KEY_VAR, top, rest));
    home
}

/// `zx-agent run` in `dir` against `server`, with the zx-agent home `home`, the API key set
/// and stdin closed; the caller adds the options and the prompt.
fn program(server: &MockServer, dir: &Path, home: &Path) -> Command {
    let mut command = zx_agent_in(dir, Home::Agent(home));
    command
        .arg("run")
        .env("ZX_BASE_URL", &server.base_url)
        .env(KEY_VAR, KEY)
        .stdin(Stdio::null());
    command
}

/// `zx-agent run` without tools against `server`.
fn run(server: &MockServer, dir: &Path) -> Command {
    run_with(server, dir, "", "")
}

/// `zx-agent run` without tools against `server`, with a config.toml in a zx-agent home of
/// `dir` that has `top` before its `[provider]` table and `rest` after the test's provider
/// keys.
fn run_with(server: &MockServer, dir: &Path, top: &str, rest: &str) -> Command {
    let mut command = program(server, dir, &home(dir, top, rest));
    command.args(["--model", "mock", "--tools", "", "go"]);
    command
}

/// `zx-agent run` against `server`, started in `dir`, with `args` after the model.
fn run_against(server: &MockServer, dir: &Path, args: &[&str]) -> Command {
    run_configured(server, dir, "", args)
}

/// Like [`run_against`], with `provider` added to the `[provider]` table of the config.toml in
/// a zx-agent home of `dir`.
fn run_configured(server: &MockServer, dir: &Path, provider: &str, args: &[&str]) -> Command {
    let mut command = program(server, dir, &home(dir, "", provider));
    command.args(["--model", "mock-model"]).args(args);
    command
}

/// Run to completion from an empty temporary directory.
async fn run_once(server: &MockServer, args: &[&str]) -> Output {
    let dir = tempfile::tempdir().unwrap();
    run_against(server, dir.path(), args).output().await.unwrap()
}

/// The `messages` array of request `index`.
fn sent_messages(server: &MockServer, index: usize) -> Vec<Value> {
    server.requests()[index].body["messages"].as_array().unwrap().clone()
}

#[tokio::test]
async fn streaming_resource_limits_reach_the_provider() {
    // One event over the provider's default limit of 1 MiB per SSE event.
    let server = MockServer::start(vec![Reply::text(&"x".repeat(1024 * 1024))]).await;
    let output = run_once(&server, &["--tools", "", "--output", "jsonl", "check"]).await;

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let events: Vec<Value> = json_lines(&stdout(&output));
    assert!(
        events
            .iter()
            .any(|event| event["message"]["providerError"]["kind"] == "resource_limit")
    );
}

#[tokio::test]
async fn repaired_session_tails_are_reported_on_stderr() {
    use std::io::Write;
    use zx_agent::ai::Message;
    use zx_agent::transcript::SessionHeader;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut session = zx_testing::create_session_file(&path, SessionHeader::new(dir.path(), "mock-model"))
        .await
        .unwrap();
    session.append_message(Message::user("earlier")).await.unwrap();
    drop(session);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"type\":\"mess")
        .unwrap();
    let server = MockServer::start(vec![Reply::text("ok")]).await;

    let output = run_against(
        &server,
        dir.path(),
        &[
            "--tools",
            "",
            "--session",
            path.to_str().unwrap(),
            "--output",
            "jsonl",
            "continue",
        ],
    )
    .output()
    .await
    .unwrap();

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stderr(&output).contains("recovered incomplete final record"));
    for line in stdout(&output).lines() {
        serde_json::from_str::<Value>(line).unwrap();
    }
}

// ---------------------------------------------------------------------------
// Exit codes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_completed_run_prints_the_answer_and_exits_0() {
    let server = MockServer::start(vec![Reply::text("All good.")]).await;

    let output = run_once(&server, &["--tools", "", "check"]).await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "All good.\n");
}

#[tokio::test]
async fn a_provider_error_exits_1() {
    let server = MockServer::start(vec![Reply::Status(401, "{\"error\":{\"message\":\"bad key\"}}")]).await;

    let output = run_once(&server, &["--tools", "", "check"]).await;

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
}

#[tokio::test]
async fn the_model_request_limit_exits_3() {
    let server = MockServer::start(vec![Reply::bash("c1", "echo hi")]).await;

    let output = run_once(&server, &["--tools", "bash", "--max-model-requests", "1", "check"]).await;

    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
}

#[tokio::test]
async fn the_time_limit_exits_4() {
    let server = MockServer::start(vec![Reply::Stall(vec![])]).await;

    let output = run_once(&server, &["--tools", "", "--timeout-secs", "1", "check"]).await;

    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_exits_5() {
    let server = MockServer::start(vec![Reply::Stall(vec![])]).await;
    let dir = tempfile::tempdir().unwrap();
    let child = run_against(&server, dir.path(), &["--tools", "", "check"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Once the request arrives, the run is in progress and its signal handler is installed.
    server.wait_for_requests(1).await;
    zx_testing::signal(&child, "-TERM").await;

    let output = ended_output(child).await;

    assert_eq!(output.status.code(), Some(5), "{}", stderr(&output));
}

#[cfg(unix)]
#[tokio::test]
async fn a_second_signal_ends_the_process_without_waiting_for_its_shutdown() {
    use std::os::unix::process::ExitStatusExt;

    let server = MockServer::start(vec![Reply::Stall(vec![])]).await;
    // The traces left at the end go to a collector that never answers: exporting them takes
    // the whole export timeout.
    let (collector, mut exports) = silent_listener().await;
    let dir = tempfile::tempdir().unwrap();
    let mut child = run_against(&server, dir.path(), &["--tools", "", "check"])
        .env(
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
            format!("http://{collector}/v1/traces"),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    server.wait_for_requests(1).await;
    zx_testing::signal(&child, "-INT").await;
    // The export has begun: the first signal has ended the run.
    exports.recv().await.unwrap();

    zx_testing::signal(&child, "-INT").await;

    let status = ended(&mut child).await;
    assert_eq!(status.signal(), Some(libc::SIGINT), "{status:?}");
}

#[tokio::test]
async fn a_truncated_answer_is_printed_and_exits_6() {
    let server = MockServer::start(vec![Reply::truncated("Part")]).await;

    let home = AgentHome::agent("unattended", "continueOnLength = 0");
    let dir = tempfile::tempdir().unwrap();
    let output = run_against(&server, dir.path(), &["--tools", "", "check"])
        .env("ZX_AGENT_HOME", home.path())
        .output()
        .await
        .unwrap();

    assert_eq!(output.status.code(), Some(6), "{}", stderr(&output));
    assert_eq!(stdout(&output), "Part\n");
}

// ---------------------------------------------------------------------------
// Input and output
// ---------------------------------------------------------------------------

#[tokio::test]
async fn jsonl_output_is_one_event_per_line_ending_with_the_report() {
    let server = MockServer::start(vec![Reply::text("All good.")]).await;

    let output = run_once(&server, &["--tools", "", "--output", "jsonl", "check"]).await;

    let events: Vec<Value> = json_lines(&stdout(&output));
    assert_eq!(events[0]["type"], "run_start");
    let report = events.last().unwrap();
    assert_eq!(report["type"], "run_end");
    assert_eq!(report["report"]["outcome"], "completed");
    assert_eq!(report["report"]["finalText"], "All good.");
}

#[tokio::test]
async fn a_dash_reads_the_prompt_from_stdin() {
    let server = MockServer::start(vec![Reply::text("ok")]).await;
    let dir = tempfile::tempdir().unwrap();
    let mut child = run_against(&server, dir.path(), &["--tools", "", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"from stdin").await.unwrap();
    // Closing stdin ends the prompt.
    drop(stdin);

    let output = child.wait_with_output().await.unwrap();

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(sent_messages(&server, 0).last().unwrap()["content"], "from stdin");
}

// ---------------------------------------------------------------------------
// Wiring done in main.rs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bash_commands_cannot_read_the_api_key() {
    let server = MockServer::start(vec![
        Reply::bash("c1", &format!("printenv {KEY_VAR}; echo status=$?")),
        Reply::text("done"),
    ])
    .await;

    let output = run_once(&server, &["--tools", "bash", "check"]).await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let tool_result = sent_messages(&server, 1)
        .into_iter()
        .find(|message| message["role"] == "tool")
        .unwrap();
    let content = tool_result["content"].as_str().unwrap();
    assert!(content.contains("status=1") && !content.contains(KEY), "{content}");
}

#[tokio::test]
async fn the_system_prompt_states_the_environment() {
    let server = MockServer::start(vec![Reply::text("ok")]).await;

    let home = AgentHome::agent("unattended", "[permissions]\nallow = [\"bash(grep)\"]");
    let dir = tempfile::tempdir().unwrap();
    run_against(&server, dir.path(), &["--tools", "bash", "check"])
        .env("ZX_AGENT_HOME", home.path())
        .output()
        .await
        .unwrap();

    let system = sent_messages(&server, 0)[0]["content"].as_str().unwrap().to_string();
    assert!(system.contains("<environment>\nOS: "), "{system}");
    // The permissions are enforced, not stated: the prompt has no command policy section.
    assert!(!system.contains("<command_policy>"), "{system}");
}

#[tokio::test]
async fn inline_updates_are_sent_where_the_prompt_changed() {
    let server = MockServer::start(vec![Reply::text("one"), Reply::text("two")]).await;
    let dir = tempfile::tempdir().unwrap();
    let session = dir.path().join("s.jsonl");
    let args = ["--tools", "", "--session", session.to_str().unwrap(), "check"];
    // Changing the agent's instructions changes the preamble.
    for instructions in ["Answer briefly.", "Answer at length."] {
        let agent = agent_text("extends = \"unattended\"", instructions);
        write_agent_file(&dir.path().join(".home"), "unattended", &agent);
        let output = run_configured(&server, dir.path(), "systemUpdates = 'inline'", &args)
            .output()
            .await
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    }

    let messages = sent_messages(&server, 1);
    let roles: Vec<&str> = messages
        .iter()
        .map(|message| message["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["system", "user", "assistant", "system", "user"]);
    let update = messages[3]["content"].as_str().unwrap();
    assert!(
        update.starts_with("Updated system prompt section \"preamble\":\n\nAnswer at length."),
        "{update}"
    );
}

/// `first` and `second` in `dir`, created, as the system resolves them.
fn two_directories(dir: &Path) -> (PathBuf, PathBuf) {
    let made = ["first", "second"].map(|name| {
        let path = dir.join(name);
        std::fs::create_dir(&path).unwrap();
        path.canonicalize().unwrap()
    });
    made.into()
}

#[tokio::test]
async fn a_continued_session_works_in_the_directory_it_started_in() {
    let server = MockServer::start(vec![Reply::text("one"), Reply::text("two")]).await;
    let dir = tempfile::tempdir().unwrap();
    let (first, second) = two_directories(dir.path());
    let session = dir.path().join("s.jsonl");
    let args = ["--tools", "", "--session", session.to_str().unwrap(), "check"];
    // Started by a process in `first`, continued by one in `second`.
    for cwd in [&first, &second] {
        let output = run_against(&server, cwd, &args).output().await.unwrap();
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    }

    let prompts: Vec<String> = sent_messages(&server, 1)
        .iter()
        .filter(|message| message["role"] == "system")
        .map(|message| message["content"].as_str().unwrap().to_string())
        .collect();
    let stated = format!("Working directory: {}", first.display());
    assert!(prompts[0].contains(&stated), "{prompts:?}");
    assert!(prompts.iter().all(|prompt| !prompt.contains("second")), "{prompts:?}");
}

#[tokio::test]
async fn continuing_a_session_with_another_cwd_is_refused_before_any_request() {
    let server = MockServer::start(vec![Reply::text("one"), Reply::text("never sent")]).await;
    let dir = tempfile::tempdir().unwrap();
    let (first, second) = two_directories(dir.path());
    let session = dir.path().join("s.jsonl");
    let run_in = |cwd: &Path| {
        let args = [
            "--tools",
            "",
            "--session",
            session.to_str().unwrap(),
            "--cwd",
            cwd.to_str().unwrap(),
            "check",
        ];
        run_against(&server, dir.path(), &args)
    };
    let started = run_in(&first).output().await.unwrap();
    assert_eq!(started.status.code(), Some(0), "{}", stderr(&started));

    let elsewhere = run_in(&second).output().await.unwrap();

    assert_eq!(elsewhere.status.code(), Some(2), "{}", stderr(&elsewhere));
    let error = stderr(&elsewhere);
    assert!(error.contains("is not where the session works"), "{error}");
    assert!(error.contains(&first.display().to_string()), "{error}");
    assert_eq!(server.requests().len(), 1);
    // The same directory, given again, continues the session.
    let again = run_in(&first).output().await.unwrap();
    assert_eq!(again.status.code(), Some(0), "{}", stderr(&again));
}

#[tokio::test]
async fn a_session_whose_directory_is_gone_is_not_continued() {
    let server = MockServer::start(vec![Reply::text("one"), Reply::text("never sent")]).await;
    let dir = tempfile::tempdir().unwrap();
    let (first, _) = two_directories(dir.path());
    let session = dir.path().join("s.jsonl");
    let args = ["--tools", "", "--session", session.to_str().unwrap(), "check"];
    let started = run_against(&server, &first, &args).output().await.unwrap();
    assert_eq!(started.status.code(), Some(0), "{}", stderr(&started));
    std::fs::remove_dir_all(&first).unwrap();

    let continued = run_against(&server, dir.path(), &args).output().await.unwrap();

    assert_eq!(continued.status.code(), Some(2), "{}", stderr(&continued));
    let error = stderr(&continued);
    assert!(
        error.contains("the session's working directory no longer exists"),
        "{error}"
    );
    assert!(error.contains(&first.display().to_string()), "{error}");
    assert_eq!(server.requests().len(), 1);
}

// ---------------------------------------------------------------------------
// Output, sessions, settings, budgets and traces
// ---------------------------------------------------------------------------

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
    let (mut terminal, slave) = pty::open();
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
    let status = ended(&mut child).await;
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
        Reply::tool_call("c1", "x\u{1b}[2J", &json!({})),
        Reply::Status(400, "bad\u{1b}[2J"),
    ])
    .await;
    let output = program(&server, dir.path(), &home(dir.path(), "", ""))
        .args(["--model", "mock", "go"])
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
    let first = ended_output(first).await;
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
    let output = program(&server, dir.path(), &home(dir.path(), "", ""))
        .args(["--model", "mock", "--tools", "bash", "go"])
        .env("OTEL_EXPORTER_OTLP_HEADERS", TOKEN)
        .env("OTEL_EXPORTER_OTLP_TRACES_HEADERS", TOKEN)
        .env("OTEL_EXPORTER_OTLP_METRICS_HEADERS", TOKEN)
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

#[tokio::test]
async fn the_output_limit_reaches_the_model_in_the_chosen_field_and_the_audit() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("ok"), Reply::text("ok")]).await;
    let cases = [
        ("", "default.jsonl", "max_tokens", "max_completion_tokens"),
        (
            "maxTokensField = 'max_completion_tokens'",
            "newer.jsonl",
            "max_completion_tokens",
            "max_tokens",
        ),
    ];
    for (index, (provider, session, sent, unsent)) in cases.into_iter().enumerate() {
        let output = run_with(&server, dir.path(), "maxTokens = 77", provider)
            .args(["--session", session])
            .output()
            .await
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        let body = &server.requests()[index].body;
        assert_eq!(body[sent], 77, "{provider}: {body}");
        assert!(body.get(unsent).is_none(), "{provider}: {body}");
    }
    let default = std::fs::read_to_string(dir.path().join("default.jsonl")).unwrap();
    assert!(!default.contains("maxTokensField"), "{default}");
    let newer = std::fs::read_to_string(dir.path().join("newer.jsonl")).unwrap();
    assert!(newer.contains(r#""maxTokensField":"max_completion_tokens""#), "{newer}");
}

/// `zx-agent run --continue` against `server`, without a prompt.
fn run_continue(server: &MockServer, dir: &Path) -> Command {
    let mut command = program(server, dir, &home(dir, "", ""));
    command.args(["--continue", "--model", "mock", "--tools", ""]);
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
        // 7 since a failed answer's `providerError` carries the kind of failure the provider
        // reported.
        assert_eq!(line["schemaVersion"], 7, "{line}");
        assert_eq!(line["seq"], index + 1, "{line}");
        assert_eq!(line["runId"], lines[0]["runId"], "{line}");
        assert!(line["sessionId"].is_string() && line["atMs"].is_u64(), "{line}");
    }
    assert_eq!(lines[0]["type"], "run_start");
    // The run's first attempt follows it.
    assert_eq!(
        (lines[1]["type"].clone(), lines[1]["attempt"].clone()),
        (json!("attempt_start"), json!(1))
    );
    let end = lines.last().unwrap();
    assert_eq!(
        (end["type"].clone(), end["report"]["finalText"].clone()),
        (json!("run_end"), json!("ok"))
    );
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
    // The answer was cut off, and continuing it needed a request the budget did not leave.
    assert!(
        stderr.contains("outcome=budget_exhausted budget=token_budget truncated=true"),
        "{stderr}"
    );
    assert!(
        stderr.contains("token budget reached (12 tokens used, token_budget = 12); the answer was cut off"),
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
    assert!(
        stderr.contains("outcome=budget_exhausted budget=cost_budget truncated=true"),
        "{stderr}"
    );
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
async fn an_error_shows_what_its_causes_quote_with_control_characters_escaped() {
    let dir = tempfile::tempdir().unwrap();
    let server = two_answers().await;

    let output = run(&server, dir.path())
        .args(["--cwd", "missing-\u{1b}[31m-dir"])
        .output()
        .await
        .unwrap();

    let stderr = stderr(&output);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("missing-\\u{1b}[31m-dir"), "{stderr}");
    assert!(!stderr.contains('\u{1b}'), "{stderr}");
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
    program(&server, dir.path(), home.path())
        .args(["--model", "mock", "--tools", "bash"])
        .args(extra)
        .arg("go")
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
    assert!(stderr.contains("outcome=completed"), "{stderr}");
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
        .env("OTEL_SERVICE_NAME", "not-a-reason-to-export")
        .output()
        .await
        .unwrap();
    let stderr = zx_testing::stderr(&output);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(!stderr.contains("[otel]"), "{stderr}");
    assert_eq!(received.lock().unwrap().len(), 1, "nothing more was sent");
}

/// The body of the one trace export of a run in `dir`, with the variables that describe the
/// resource set to `resource` and the other variables of a trace unset.
async fn exported_trace(dir: &Path, resource: &[(&str, &str)]) -> Vec<u8> {
    let (endpoint, received) = collector().await;
    let server = MockServer::start(vec![Reply::text("the answer")]).await;
    let mut command = run(&server, dir);
    command.env("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", format!("{endpoint}/v1/traces"));
    for (name, value) in resource {
        command.env(name, value);
    }
    let output = command.output().await.unwrap();
    let stderr = stderr(&output);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    let mut requests = received.lock().unwrap();
    assert_eq!(requests.len(), 1, "one export, at the end");
    requests.remove(0).body
}

/// The string value of the attribute `key` in an OTLP/protobuf export, the first one when
/// several have that key. An attribute is a `KeyValue`: the key as field 1, then the value as
/// field 2, an `AnyValue` whose field 1 is a string. Each field is its tag, its length (one
/// byte for keys and values as short as these tests') and its bytes.
fn attribute(body: &[u8], key: &str) -> Option<String> {
    let mut key_field = vec![0x0a, u8::try_from(key.len()).unwrap()];
    key_field.extend_from_slice(key.as_bytes());
    key_field.push(0x12);
    let at = body.windows(key_field.len()).position(|window| window == key_field)? + key_field.len();
    let [_, 0x0a, length, rest @ ..] = body.get(at..)? else {
        return None;
    };
    let value = rest.get(..usize::from(*length))?;
    Some(String::from_utf8(value.to_vec()).unwrap())
}

#[tokio::test]
async fn otel_service_name_names_the_service_over_the_resource_attributes() {
    let dir = tempfile::tempdir().unwrap();
    for resource in [
        vec![("OTEL_SERVICE_NAME", "from-the-variable")],
        vec![
            ("OTEL_SERVICE_NAME", "from-the-variable"),
            ("OTEL_RESOURCE_ATTRIBUTES", "service.name=from-the-attributes"),
        ],
    ] {
        let body = exported_trace(dir.path(), &resource).await;
        assert_eq!(
            attribute(&body, "service.name").as_deref(),
            Some("from-the-variable"),
            "{resource:?}"
        );
    }
}

#[tokio::test]
async fn a_service_name_in_the_resource_attributes_comes_before_the_programs_name() {
    let dir = tempfile::tempdir().unwrap();
    let resource = "service.name=from-the-attributes,deployment.environment.name=staging";

    let body = exported_trace(dir.path(), &[("OTEL_RESOURCE_ATTRIBUTES", resource)]).await;

    assert_eq!(attribute(&body, "service.name").as_deref(), Some("from-the-attributes"));
    // The other attributes stay: the user's, and the SDK's own.
    assert_eq!(
        attribute(&body, "deployment.environment.name").as_deref(),
        Some("staging")
    );
    assert_eq!(attribute(&body, "telemetry.sdk.language").as_deref(), Some("rust"));
}

#[tokio::test]
async fn the_service_takes_the_programs_name_when_the_environment_names_none() {
    let dir = tempfile::tempdir().unwrap();

    let body = exported_trace(dir.path(), &[]).await;

    assert_eq!(attribute(&body, "service.name").as_deref(), Some("zx-agent"));
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
        .env("TRACEPARENT", "not-a-traceparent")
        .output()
        .await
        .unwrap();
    let stderr = zx_testing::stderr(&output);
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

#[tokio::test]
async fn a_closed_stderr_leaves_the_exit_code_to_the_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("the answer")]).await;
    let mut child = run(&server, dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Whoever was to read the progress and the report line is gone before the run starts.
    drop(child.stderr.take());

    let output = child.wait_with_output().await.unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(stdout(&output), "the answer\n");
}
