//! The command-line program, run as a separate process against the local fake server: exit
//! codes, output modes, and the wiring that only main.rs does. No network or API key needed.

mod common;

use std::path::Path;
use std::process::{Output, Stdio};

use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use common::mock_server::{MockServer, Reply};

/// The variable the program reads the API key from, and the fake key it holds.
const KEY_VAR: &str = "ZX_CLI_TEST_KEY";
const KEY: &str = "sk-cli-test-key";
/// A port nothing listens on, for runs that must fail before any request.
const NO_SERVER: &str = "http://127.0.0.1:9/v1";

/// `zx-agent run` against `base_url`, started in `dir`, with `args` after the common options.
fn zx_agent(base_url: &str, dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zx-agent"));
    command
        .arg("run")
        .args(["--model", "mock-model", "--base-url", base_url])
        .args(["--api-key-env", KEY_VAR, "--max-retries", "0"])
        .args(args)
        .current_dir(dir)
        .env(KEY_VAR, KEY)
        // Settings from the developer's shell must not change the test.
        .env_remove("ZX_MODEL")
        .env_remove("ZX_BASE_URL")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    command
}

/// Run to completion from an empty temporary directory.
async fn run(base_url: &str, args: &[&str]) -> Output {
    let dir = tempfile::tempdir().unwrap();
    zx_agent(base_url, dir.path(), args).output().await.unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The `messages` array of request `index`.
fn sent_messages(server: &MockServer, index: usize) -> Vec<Value> {
    server.requests()[index].body["messages"].as_array().unwrap().clone()
}

// ---------------------------------------------------------------------------
// Exit codes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_completed_run_prints_the_answer_and_exits_0() {
    let server = MockServer::start(vec![Reply::text("All good.")]).await;

    let output = run(&server.base_url, &["--tools", "", "check"]).await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "All good.\n");
}

#[tokio::test]
async fn a_provider_error_exits_1() {
    let server = MockServer::start(vec![Reply::Status(401, "{\"error\":{\"message\":\"bad key\"}}")]).await;

    let output = run(&server.base_url, &["--tools", "", "check"]).await;

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
}

#[tokio::test]
async fn a_configuration_error_exits_2_before_any_request() {
    let output = run(NO_SERVER, &["--tools", "curl", "check"]).await;

    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("unknown tool \"curl\""), "{}", stderr(&output));
}

#[tokio::test]
async fn the_turn_limit_exits_3() {
    let server = MockServer::start(vec![Reply::bash("c1", "echo hi")]).await;

    let output = run(&server.base_url, &["--tools", "bash", "--max-turns", "1", "check"]).await;

    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
}

#[tokio::test]
async fn the_time_limit_exits_4() {
    let server = MockServer::start(vec![Reply::Stall(vec![])]).await;

    let output = run(&server.base_url, &["--tools", "", "--timeout-secs", "1", "check"]).await;

    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_exits_5() {
    let server = MockServer::start(vec![Reply::Stall(vec![])]).await;
    let dir = tempfile::tempdir().unwrap();
    let child = zx_agent(&server.base_url, dir.path(), &["--tools", "", "check"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Once the request arrives, the run is in progress and its signal handler is installed.
    server.wait_for_requests(1).await;
    let pid = child.id().unwrap().to_string();
    let killed = Command::new("kill").args(["-TERM", &pid]).status().await.unwrap();
    assert!(killed.success());

    let output = child.wait_with_output().await.unwrap();

    assert_eq!(output.status.code(), Some(5), "{}", stderr(&output));
}

#[tokio::test]
async fn a_truncated_answer_is_printed_and_exits_6() {
    let server = MockServer::start(vec![Reply::truncated("Part")]).await;

    let output = run(&server.base_url, &["--tools", "", "--continue-on-length", "0", "check"]).await;

    assert_eq!(output.status.code(), Some(6), "{}", stderr(&output));
    assert_eq!(stdout(&output), "Part\n");
}

// ---------------------------------------------------------------------------
// Input and output
// ---------------------------------------------------------------------------

#[tokio::test]
async fn jsonl_output_is_one_event_per_line_ending_with_the_report() {
    let server = MockServer::start(vec![Reply::text("All good.")]).await;

    let output = run(&server.base_url, &["--tools", "", "--output", "jsonl", "check"]).await;

    let events: Vec<Value> = stdout(&output)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events[0]["type"], "agent_start");
    let report = events.last().unwrap();
    assert_eq!(report["type"], "run_end");
    assert_eq!(report["report"]["outcome"], "completed");
    assert_eq!(report["report"]["finalText"], "All good.");
}

#[tokio::test]
async fn a_dash_reads_the_prompt_from_stdin() {
    let server = MockServer::start(vec![Reply::text("ok")]).await;
    let dir = tempfile::tempdir().unwrap();
    let mut child = zx_agent(&server.base_url, dir.path(), &["--tools", "", "-"])
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
        Reply::bash("c1", "printenv ZX_CLI_TEST_KEY; echo status=$?"),
        Reply::text("done"),
    ])
    .await;

    let output = run(&server.base_url, &["--tools", "bash", "check"]).await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let tool_result = sent_messages(&server, 1)
        .into_iter()
        .find(|message| message["role"] == "tool")
        .unwrap();
    let content = tool_result["content"].as_str().unwrap();
    assert!(content.contains("status=1") && !content.contains(KEY), "{content}");
}

#[tokio::test]
async fn the_system_prompt_states_the_environment_and_command_policy() {
    let server = MockServer::start(vec![Reply::text("ok")]).await;

    run(&server.base_url, &["--tools", "bash", "--allow-cmd", "grep", "check"]).await;

    let system = sent_messages(&server, 0)[0]["content"].as_str().unwrap().to_string();
    assert!(system.contains("<environment>\nOS: "), "{system}");
    assert!(system.contains("must start with one of: grep."), "{system}");
}

#[tokio::test]
async fn inline_updates_are_sent_where_the_prompt_changed() {
    let server = MockServer::start(vec![Reply::text("one"), Reply::text("two")]).await;
    let dir = tempfile::tempdir().unwrap();
    let session = dir.path().join("s.jsonl");
    let session = session.to_str().unwrap();
    // Changing the working directory changes the <environment> section.
    for name in ["first", "second"] {
        let cwd = dir.path().join(name);
        std::fs::create_dir(&cwd).unwrap();
        let args = [
            "--tools",
            "",
            "--system-updates",
            "inline",
            "--session",
            session,
            "--cwd",
            cwd.to_str().unwrap(),
            "check",
        ];
        let output = zx_agent(&server.base_url, dir.path(), &args).output().await.unwrap();
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
        update.starts_with("Updated system prompt section \"environment\""),
        "{update}"
    );
}
