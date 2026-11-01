//! `run` output contracts added after P0, against the local fake HTTP service. The original
//! run tests stay byte-identical in tests/cli.rs.
mod common;

use std::process::Stdio;
use std::time::Duration;

use serde_json::json;
use tokio::process::Command;

use common::mock_server::{MockServer, Reply};

/// An answer that would set the terminal title if a terminal ran it.
const TITLE_ANSWER: &str = "before\u{1b}]0;CANARY\u{7}after";

/// `zx-agent run` without tools against `server`.
fn run(server: &MockServer, dir: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zx-agent"));
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
    assert_eq!(String::from_utf8_lossy(&output.stdout), format!("{TITLE_ANSWER}\n"));
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
    let output = Command::new(env!("CARGO_BIN_EXE_zx-agent"))
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
    let stderr = String::from_utf8_lossy(&output.stderr);
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

    let stderr = String::from_utf8_lossy(&second.stderr);
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
    let output = Command::new(env!("CARGO_BIN_EXE_zx-agent"))
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
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result = server.requests()[1].body["messages"].to_string();
    assert!(result.contains("Permission denied"), "{result}");
    assert!(!result.contains(PROBE), "{result}");
}
