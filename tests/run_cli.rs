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
