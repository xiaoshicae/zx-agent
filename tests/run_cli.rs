//! `run` output contracts added after P0, against the local fake HTTP service. The original
//! run tests stay byte-identical in tests/cli.rs.
mod common;

use serde_json::json;
use tokio::process::Command;

use common::mock_server::{MockServer, Reply};

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
