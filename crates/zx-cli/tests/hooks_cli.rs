//! Hook wiring in the CLI, exercised only against the local mock server.

mod common;

use std::path::Path;

use serde_json::json;
use tokio::process::Command;

use common::mock_server::{MockServer, Reply};
use common::{AgentHome, Home, fake_model_config, stderr, stdout, write_config, zx_agent_in};

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

    let home = AgentHome::new(&fake_model_config(KEY_VAR, "", ""));
    let output = zx_agent_in(dir.path(), Home::Agent(home.path()))
        .args(["run", "--model", "mock", "--tools", "read_file"])
        .args([
            "--output",
            "jsonl",
            "--session",
            session.to_str().unwrap(),
            "read credentials.txt",
        ])
        .env("ZX_BASE_URL", &server.base_url)
        .env(KEY_VAR, KEY)
        .output()
        .await
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let requests = server.requests();
    let messages = requests[1].body["messages"].to_string();
    assert!(!messages.contains(KEY), "raw key reached the next model request");
    assert!(messages.contains("[REDACTED]"));
    assert!(!stdout(&output).contains(KEY));
    assert!(!std::fs::read_to_string(session).unwrap().contains(KEY));
}

/// `zx-agent run` with bash against `server`, in `dir`.
fn run_bash(server: &MockServer, dir: &Path) -> Command {
    run_bash_redacting(server, dir, &[])
}

/// `zx-agent run` with bash against `server`, in `dir`, with `secret_files` in the
/// `secretFiles` of a config.toml in a zx-agent home of `dir`.
fn run_bash_redacting(server: &MockServer, dir: &Path, secret_files: &[&Path]) -> Command {
    let home = dir.join(".home");
    let files: Vec<String> = secret_files
        .iter()
        .map(|path| format!("{:?}", path.display().to_string()))
        .collect();
    let top = format!("secretFiles = [{}]", files.join(", "));
    write_config(&home, &fake_model_config("ZX_HOOKS_CLI_TEST_KEY", &top, ""));
    let mut command = zx_agent_in(dir, Home::Agent(&home));
    command
        .args(["run", "--model", "mock", "--tools", "bash", "go"])
        .env("ZX_BASE_URL", &server.base_url)
        .env_remove("ZX_SECRET_FILES");
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

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let messages = server.requests()[1].body["messages"].to_string();
    assert!(!messages.contains("remote-token-a"), "{messages}");
    assert!(!messages.contains("metrics-token-b"), "{messages}");
    assert_eq!(messages.matches("[REDACTED]").count(), 2, "{messages}");
}

#[tokio::test]
async fn secret_files_of_config_toml_add_to_those_in_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    let from_env = dir.path().join("env-token");
    std::fs::write(&from_env, "token-from-env").unwrap();
    let from_flag = dir.path().join("flag-token");
    std::fs::write(&from_flag, "token-from-flag").unwrap();
    let server = MockServer::start(vec![Reply::bash("c", "cat env-token flag-token"), Reply::text("done")]).await;

    let output = run_bash_redacting(&server, dir.path(), &[&from_flag])
        .env("ZX_SECRET_FILES", &from_env)
        .output()
        .await
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let messages = server.requests()[1].body["messages"].to_string();
    assert!(!messages.contains("token-from-env"), "{messages}");
    assert!(!messages.contains("token-from-flag"), "{messages}");
}

#[tokio::test]
async fn a_missing_secret_file_is_a_configuration_error() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("never")]).await;
    let missing = dir.path().join("no-such-token");

    let output = run_bash_redacting(&server, dir.path(), &[&missing])
        .output()
        .await
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = stderr(&output);
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

    let output = run_bash_redacting(&server, dir.path(), &[&secrets])
        .output()
        .await
        .unwrap();

    let stderr = stderr(&output);
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

    let output = run_bash_redacting(&server, dir.path(), &[&token])
        .output()
        .await
        .unwrap();

    let stderr = stderr(&output);
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

    let output = run_bash_redacting(&server, dir.path(), &[&token])
        .args(["--output", "jsonl"])
        .output()
        .await
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let failures: Vec<(String, String)> = stdout(&output)
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

    let output = run_bash_redacting(&server, dir.path(), &[&secrets])
        .output()
        .await
        .unwrap();

    let stderr = stderr(&output);
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
