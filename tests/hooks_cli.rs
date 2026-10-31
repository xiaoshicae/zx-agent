//! Hook wiring in the CLI, exercised only against the local mock server.

mod common;

use serde_json::json;
use tokio::process::Command;

use common::mock_server::{MockServer, Reply};

#[tokio::test]
async fn known_api_key_is_redacted_before_model_events_and_session() {
    const KEY: &str = "sk-p1-test-secret";
    const KEY_VAR: &str = "ZX_P1_TEST_KEY";
    let dir = tempfile::tempdir().unwrap();
    let session = dir.path().join("session.jsonl");
    std::fs::write(dir.path().join("credentials.txt"), format!("token={KEY}\n")).unwrap();
    let server = MockServer::start(vec![
        Reply::Sse(vec![json!({"choices": [{"index": 0, "delta": {"tool_calls": [{
            "index": 0, "id": "read1", "type": "function",
            "function": {"name": "read_file", "arguments": "{\"path\":\"credentials.txt\"}"}
        }]}, "finish_reason": "tool_calls"}]})]),
        Reply::text("done"),
    ])
    .await;

    let output = Command::new(env!("CARGO_BIN_EXE_zx-agent"))
        .args(["run", "--model", "mock", "--base-url", &server.base_url])
        .args(["--tools", "read_file", "--api-key-env", KEY_VAR, "--max-retries", "0"])
        .args([
            "--output",
            "jsonl",
            "--session",
            session.to_str().unwrap(),
            "read credentials.txt",
        ])
        .current_dir(dir.path())
        .env(KEY_VAR, KEY)
        .env_remove("ZX_MODEL")
        .env_remove("ZX_BASE_URL")
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
    let requests = server.requests();
    let messages = requests[1].body["messages"].to_string();
    assert!(!messages.contains(KEY), "raw key reached the next model request");
    assert!(messages.contains("[REDACTED]"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains(KEY));
    assert!(!std::fs::read_to_string(session).unwrap().contains(KEY));
}
