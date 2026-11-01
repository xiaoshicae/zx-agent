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

/// `zx-agent run` with bash against `server`, in `dir`.
fn run_bash(server: &MockServer, dir: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zx-agent"));
    command
        .args(["run", "--model", "mock", "--base-url", &server.base_url])
        .args(["--tools", "bash", "--max-retries", "0", "go"])
        .current_dir(dir)
        .env_remove("ZX_MODEL")
        .env_remove("ZX_BASE_URL")
        .env_remove("ZX_SECRET_FILES")
        .kill_on_drop(true);
    command
}

#[tokio::test]
async fn secret_files_named_in_the_environment_are_redacted() {
    let dir = tempfile::tempdir().unwrap();
    let clusters = dir.path().join("clusters");
    std::fs::create_dir(&clusters).unwrap();
    std::fs::write(clusters.join("cluster-a"), "remote-token-a\n").unwrap();
    let metrics = dir.path().join("metrics-token");
    std::fs::write(&metrics, "metrics-token-b").unwrap();
    let server = MockServer::start(vec![
        Reply::bash("c", "cat clusters/cluster-a metrics-token"),
        Reply::text("done"),
    ])
    .await;

    let output = run_bash(&server, dir.path())
        .env(
            "ZX_SECRET_FILES",
            format!("{},{}", clusters.display(), metrics.display()),
        )
        .output()
        .await
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let messages = server.requests()[1].body["messages"].to_string();
    assert!(!messages.contains("remote-token-a"), "{messages}");
    assert!(!messages.contains("metrics-token-b"), "{messages}");
    assert_eq!(messages.matches("[REDACTED]").count(), 2, "{messages}");
}

#[tokio::test]
async fn secret_files_from_the_flag_add_to_those_in_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    let from_env = dir.path().join("env-token");
    std::fs::write(&from_env, "token-from-env").unwrap();
    let from_flag = dir.path().join("flag-token");
    std::fs::write(&from_flag, "token-from-flag").unwrap();
    let server = MockServer::start(vec![Reply::bash("c", "cat env-token flag-token"), Reply::text("done")]).await;

    let output = run_bash(&server, dir.path())
        .env("ZX_SECRET_FILES", &from_env)
        .args(["--secret-file", from_flag.to_str().unwrap()])
        .output()
        .await
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let messages = server.requests()[1].body["messages"].to_string();
    assert!(!messages.contains("token-from-env"), "{messages}");
    assert!(!messages.contains("token-from-flag"), "{messages}");
}

#[tokio::test]
async fn a_missing_secret_file_is_a_configuration_error() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("never")]).await;
    let missing = dir.path().join("no-such-token");

    let output = run_bash(&server, dir.path())
        .args(["--secret-file", missing.to_str().unwrap()])
        .output()
        .await
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no-such-token"), "{stderr}");
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn a_secret_file_too_large_to_match_is_a_configuration_error() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = dir.path().join("secrets");
    std::fs::create_dir(&secrets).unwrap();
    std::fs::write(secrets.join("token"), "token-a").unwrap();
    // Larger than any one tool output, so it could never be matched whole.
    std::fs::write(secrets.join("bundle"), "x".repeat(64 * 1024 + 1)).unwrap();
    let server = MockServer::start(vec![Reply::text("never")]).await;

    let output = run_bash(&server, dir.path())
        .args(["--secret-file", secrets.to_str().unwrap()])
        .output()
        .await
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains(&secrets.join("bundle").display().to_string()),
        "{stderr}"
    );
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn a_failed_redaction_is_reported_in_text_output() {
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    std::fs::write(&token, "token-value").unwrap();
    // The command grows the secret file past the limit: redaction fails and discards the
    // output.
    let server = MockServer::start(vec![
        Reply::bash("c", "head -c 70000 /dev/zero | tr '\\0' x > token; echo hi"),
        Reply::text("done"),
    ])
    .await;

    let output = run_bash(&server, dir.path())
        .args(["--secret-file", token.to_str().unwrap()])
        .output()
        .await
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    let reported = format!(
        "[hook] redact_secrets after_tool failed: cannot read a secret file to redact tool output: {} exceeds 65536 bytes",
        token.display()
    );
    assert!(stderr.lines().any(|line| line.starts_with(&reported)), "{stderr}");
}

#[tokio::test]
async fn hook_failures_name_the_built_in_hook_in_jsonl_events() {
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    std::fs::write(&token, "token-value").unwrap();
    let server = MockServer::start(vec![
        Reply::bash("c", "head -c 70000 /dev/zero | tr '\\0' x > token; echo hi"),
        Reply::text("done"),
    ])
    .await;

    let output = run_bash(&server, dir.path())
        .args(["--secret-file", token.to_str().unwrap(), "--output", "jsonl"])
        .output()
        .await
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let failures: Vec<(String, String)> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|event| event["type"] == "hook_failed")
        .map(|event| {
            (
                event["hook"].as_str().unwrap().to_string(),
                event["phase"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    // A fixed name, not the type name, whose form Rust does not guarantee.
    assert_eq!(failures, [("redact_secrets".to_string(), "after_tool".to_string())]);
}

#[tokio::test]
async fn a_short_secret_is_reported_at_startup_and_still_redacted() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = dir.path().join("secrets");
    std::fs::create_dir(&secrets).unwrap();
    std::fs::write(secrets.join("username"), "admin\n").unwrap();
    std::fs::write(secrets.join("token"), "token-value-long-enough").unwrap();
    let server = MockServer::start(vec![
        Reply::bash("c", "printf 'user=%s\\n' adm''in"),
        Reply::text("done"),
    ])
    .await;

    let output = run_bash(&server, dir.path())
        .args(["--secret-file", secrets.to_str().unwrap()])
        .output()
        .await
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    let notice: Vec<&str> = stderr.lines().filter(|line| line.starts_with("[secret] ")).collect();
    assert_eq!(notice.len(), 1, "{stderr}");
    assert!(
        notice[0].starts_with(&format!("[secret] {}: ", secrets.join("username").display())),
        "{stderr}"
    );
    assert!(notice[0].contains("shorter than 8 characters"), "{stderr}");
    // The notice never shows the value itself, and the value is still redacted.
    assert!(!notice[0].contains("admin"), "{stderr}");
    let messages = server.requests()[1].body["messages"].to_string();
    assert!(messages.contains("user=[REDACTED]"), "{messages}");
}
