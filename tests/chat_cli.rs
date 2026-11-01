//! Interactive frontend contracts against the local fake HTTP service; no paid APIs.
mod common;

use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdout, Command};

use common::mock_server::{MockServer, Reply};
use zx_agent::ai::Message;
use zx_agent::session::SessionStore;

const KEY_VAR: &str = "ZX_CHAT_TEST_KEY";
const KEY: &str = "sk-chat-test-key";

fn chat(server: &MockServer, cwd: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zx-agent"));
    command
        .arg("chat")
        .args(["--model", "mock-model", "--base-url", &server.base_url])
        .args([
            "--api-key-env",
            KEY_VAR,
            "--max-retries",
            "0",
            "--idle-timeout-secs",
            "10",
        ])
        .args(args)
        .current_dir(cwd)
        .env(KEY_VAR, KEY)
        .env_remove("ZX_MODEL")
        .env_remove("ZX_BASE_URL")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

async fn run(server: &MockServer, cwd: &Path, args: &[&str], input: &str) -> Output {
    let mut child = chat(server, cwd, args).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(input.as_bytes()).await.unwrap();
    stdin.shutdown().await.unwrap();
    drop(stdin);
    tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .unwrap()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn users(server: &MockServer, index: usize) -> Vec<String> {
    server.requests()[index].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "user")
        .map(|message| message["content"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn multiple_prompts_keep_history_and_stream_each_answer_once() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("first answer"), Reply::text("second answer")]).await;
    let output = run(&server, dir.path(), &["--tools", ""], "你好\nsecond\n/quit\n").await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "first answer\nsecond answer\n");
    assert_eq!(users(&server, 1), ["你好", "second"]);
    let system = server.requests()[0].body["messages"][0]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(system.contains("working with a human"));
    assert!(!system.contains("No one will answer"));
}

#[tokio::test]
async fn slash_commands_are_local_and_compact_is_explicitly_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![]).await;
    let output = run(
        &server,
        dir.path(),
        &["--tools", ""],
        "/help\n/model\n/compact\n/unknown\n\n/quit\n",
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(server.requests().is_empty());
    assert!(stderr(&output).contains("model=mock-model"));
    assert!(stderr(&output).contains("planned for P5"));
    assert!(stderr(&output).contains("unknown command"));
}

#[tokio::test]
async fn an_oversized_input_line_is_rejected_and_the_conversation_continues() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("still here")]).await;
    // One byte over the 1 MiB line limit, followed by an ordinary prompt.
    let input = format!("{}\n你好\n/quit\n", "x".repeat(1024 * 1024 + 1));
    let output = run(&server, dir.path(), &["--tools", ""], &input).await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stderr(&output).contains("exceeds"), "{}", stderr(&output));
    assert_eq!(users(&server, 0), ["你好"]);
    assert_eq!(stdout(&output), "still here\n");
}

#[tokio::test]
async fn model_command_changes_subsequent_requests_without_losing_history() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("one"), Reply::text("two")]).await;
    let output = run(
        &server,
        dir.path(),
        &["--tools", ""],
        "first\n/model next-model\nsecond\n/quit\n",
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(server.requests()[1].body["model"], "next-model");
    assert_eq!(users(&server, 1), ["first", "second"]);
}

#[tokio::test]
async fn new_command_starts_an_empty_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("one"), Reply::text("two")]).await;
    let output = run(&server, dir.path(), &["--tools", ""], "first\n/new\nsecond\n/quit\n").await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(users(&server, 1), ["second"]);
}

#[tokio::test]
async fn new_command_creates_a_separate_audit_file_and_preserves_the_old_one() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("one"), Reply::text("two")]).await;
    let output = run(
        &server,
        dir.path(),
        &["--tools", "", "--session", "chat.jsonl"],
        "first\n/new\nsecond\n/quit\n",
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let old = SessionStore::open(&dir.path().join("chat.jsonl")).unwrap();
    let old_users: Vec<_> = old
        .messages()
        .into_iter()
        .filter_map(|message| match message {
            Message::User { content, .. } => Some(content),
            _ => None,
        })
        .collect();
    assert_eq!(old_users, ["first"]);
    let files: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "jsonl"))
        .collect();
    assert_eq!(files.len(), 2);
    let next = files
        .iter()
        .find(|path| path.file_name().unwrap() != "chat.jsonl")
        .unwrap();
    let next_users: Vec<_> = SessionStore::open(next)
        .unwrap()
        .messages()
        .into_iter()
        .filter_map(|message| match message {
            Message::User { content, .. } => Some(content),
            _ => None,
        })
        .collect();
    assert_eq!(next_users, ["second"]);
}

#[tokio::test]
async fn approving_once_allows_the_displayed_shell_call() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::bash("c", "printf approved > marker"), Reply::text("done")]).await;
    let output = run(
        &server,
        dir.path(),
        &["--tools", "bash"],
        "go\n/approve 1 once\n/quit\n",
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(std::fs::read_to_string(dir.path().join("marker")).unwrap(), "approved");
    assert!(stderr(&output).contains("[approval 1]"));
    assert!(stderr(&output).contains("printf approved > marker"));
    assert!(stderr(&output).contains("[reason]"));
    assert!(stderr(&output).contains("[scope]"));
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn a_shell_call_waits_for_an_approval_before_any_side_effect() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::bash("c", "printf approved > marker"), Reply::text("done")]).await;
    let mut child = chat(&server, dir.path(), &["--tools", "bash"]).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut errors = BufReader::new(child.stderr.take().unwrap()).lines();
    stdin.write_all(b"go\n").await.unwrap();
    read_error_until(&mut errors, "[approval] /approve 1").await;
    assert!(!dir.path().join("marker").exists());
    stdin.write_all(b"/approve 1 once\n/quit\n").await.unwrap();
    drop(stdin);
    let result = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status.code(), Some(0));
    assert!(dir.path().join("marker").exists());
}

#[tokio::test]
async fn session_approval_covers_only_repeated_identical_calls() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![
        Reply::bash("c1", "printf x >> marker"),
        Reply::bash("c2", "printf x >> marker"),
        Reply::text("done"),
    ])
    .await;
    let output = run(
        &server,
        dir.path(),
        &["--tools", "bash"],
        "go\n/approve 1 session\n/quit\n",
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(std::fs::read_to_string(dir.path().join("marker")).unwrap(), "xx");
    assert!(stdout(&output).contains("done"));
    assert!(!stderr(&output).contains("[approval 2]"));
}

#[tokio::test]
async fn changed_arguments_need_a_new_approval_after_a_session_grant() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![
        Reply::bash("c1", "printf x > first"),
        Reply::bash("c2", "printf y > second"),
        Reply::text("done"),
    ])
    .await;
    let output = run(
        &server,
        dir.path(),
        &["--tools", "bash"],
        "go\n/approve 1 session\n/deny 2 changed\n/quit\n",
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(dir.path().join("first").exists());
    assert!(!dir.path().join("second").exists());
    assert!(stderr(&output).contains("[approval 2]"));
    assert_eq!(server.requests().len(), 3);
}

#[tokio::test]
async fn denial_reaches_the_model_without_executing_the_call() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::bash("c", "printf no > marker"), Reply::text("handled")]).await;
    let output = run(
        &server,
        dir.path(),
        &["--tools", "bash"],
        "go\n/deny 1 not authorized\n/quit\n",
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!dir.path().join("marker").exists());
    let body = server.requests()[1].body["messages"].to_string();
    assert!(body.contains("not authorized"));
}

#[tokio::test]
async fn a_policy_denial_cannot_be_overridden_by_an_approval_command() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![
        Reply::bash("c", "printf blocked > marker"),
        Reply::text("handled"),
    ])
    .await;
    let output = run(
        &server,
        dir.path(),
        &["--tools", "bash", "--deny-cmd", "blocked"],
        "go\n/approve 1 once\n/quit\n",
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!dir.path().join("marker").exists());
    assert!(!stderr(&output).contains("[approval 1]"));
    assert!(
        server.requests()[1].body["messages"]
            .to_string()
            .contains("Blocked by policy")
    );
}

#[tokio::test]
async fn eof_during_approval_never_executes_the_tool_or_hangs_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::bash("c", "printf no > marker")]).await;
    let output = run(&server, dir.path(), &["--tools", "bash"], "go\n").await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!dir.path().join("marker").exists());
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn stale_approval_ids_do_not_authorize_the_current_call() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::bash("c", "printf no > marker"), Reply::text("handled")]).await;
    let output = run(
        &server,
        dir.path(),
        &["--tools", "bash"],
        "go\n/approve 99 once\n/deny 1\n/quit\n",
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!dir.path().join("marker").exists());
    assert!(stderr(&output).contains("request 99 is not pending"));
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn new_conversations_drop_grants_and_do_not_reuse_approval_ids() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![
        Reply::bash("c1", "printf x >> marker"),
        Reply::text("first done"),
        Reply::bash("c2", "printf x >> marker"),
        Reply::text("second done"),
    ])
    .await;
    let input = "first\n/approve 1 session\n/new\nsecond\n/deny 2\n/quit\n";
    let output = run(&server, dir.path(), &["--tools", "bash"], input).await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(std::fs::read_to_string(dir.path().join("marker")).unwrap(), "x");
    assert!(stderr(&output).contains("[approval 2]"));
}

#[tokio::test]
async fn truncated_answers_are_shown_without_automatic_continuation() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::truncated("partial"), Reply::text("never")]).await;
    let output = run(&server, dir.path(), &["--tools", ""], "go\n/quit\n").await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "partial\n");
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn zero_turn_budget_reports_the_limit_and_keeps_the_repl_available() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![]).await;
    let output = run(
        &server,
        dir.path(),
        &["--tools", "", "--max-turns", "0", "--output", "jsonl"],
        "go\n/quit\n",
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let reports: Vec<Value> = stdout(&output)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .filter(|event: &Value| event["type"] == "run_end")
        .collect();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0]["report"]["outcome"], "max_turns");
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn provider_error_control_sequences_are_displayed_literally() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::Status(400, "bad\u{1b}[2J")]).await;
    let output = run(&server, dir.path(), &["--tools", ""], "go\n/quit\n").await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        !stderr(&output).contains('\u{1b}'),
        "raw terminal escape reached stderr"
    );
    assert!(stderr(&output).contains("\\u{1b}"));
}

#[tokio::test]
async fn chat_filters_the_known_api_key_before_tool_results_reach_audit_and_model() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("credentials.txt"), KEY).unwrap();
    let tool = json!({"choices":[{"index":0,"delta":{"tool_calls":[{
        "index":0,"id":"read","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"credentials.txt\"}"}
    }]},"finish_reason":"tool_calls"}]});
    let server = MockServer::start(vec![Reply::Sse(vec![tool]), Reply::text("done")]).await;
    let output = run(
        &server,
        dir.path(),
        &["--tools", "read_file", "--session", "chat.jsonl"],
        "read\n/quit\n",
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let body = server.requests()[1].body["messages"].to_string();
    assert!(!body.contains(KEY));
    assert!(body.contains("[REDACTED]"));
    assert!(
        !std::fs::read_to_string(dir.path().join("chat.jsonl"))
            .unwrap()
            .contains(KEY)
    );
}

async fn read_event_until(lines: &mut Lines<BufReader<ChildStdout>>, predicate: impl Fn(&Value) -> bool) -> Value {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let line = lines.next_line().await.unwrap().expect("event stream ended early");
            let event: Value = serde_json::from_str(&line).unwrap();
            if predicate(&event) {
                break event;
            }
        }
    })
    .await
    .unwrap()
}

async fn read_error_until(lines: &mut Lines<BufReader<tokio::process::ChildStderr>>, needle: &str) {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let line = lines.next_line().await.unwrap().expect("stderr ended early");
            if line.contains(needle) {
                break;
            }
        }
    })
    .await
    .unwrap()
}

#[cfg(unix)]
async fn signal(child: &Child, name: &str) {
    assert!(
        Command::new("kill")
            .args([name, &child.id().unwrap().to_string()])
            .status()
            .await
            .unwrap()
            .success()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn streamed_output_is_visible_before_cancel_and_the_conversation_can_continue() {
    let dir = tempfile::tempdir().unwrap();
    let partial = json!({"choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":null}]});
    let server = MockServer::start(vec![Reply::Stall(vec![partial]), Reply::text("after cancellation")]).await;
    let mut child = chat(&server, dir.path(), &["--tools", "", "--output", "jsonl", "--deltas"])
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut events = BufReader::new(child.stdout.take().unwrap()).lines();
    stdin.write_all(b"first\n").await.unwrap();
    read_event_until(&mut events, |event| event["type"] == "message_update").await;
    assert!(child.try_wait().unwrap().is_none(), "stream must still be active");
    signal(&child, "-INT").await;
    let aborted = read_event_until(&mut events, |event| event["type"] == "run_end").await;
    assert_eq!(aborted["report"]["outcome"], "aborted");
    stdin.write_all(b"second\n/quit\n").await.unwrap();
    drop(stdin);
    let next = read_event_until(&mut events, |event| event["type"] == "run_end").await;
    assert_eq!(next["report"]["outcome"], "completed");
    assert_eq!(next["report"]["finalText"], "after cancellation");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .code(),
        Some(0)
    );
    assert_eq!(users(&server, 1), ["first", "second"]);
}

#[cfg(unix)]
#[tokio::test]
async fn cancelling_an_approval_drops_its_reply_and_allows_a_new_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::bash("c", "printf no > marker"), Reply::text("safe")]).await;
    let mut child = chat(&server, dir.path(), &["--tools", "bash", "--output", "jsonl"])
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut events = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut errors = BufReader::new(child.stderr.take().unwrap()).lines();
    stdin.write_all(b"first\n").await.unwrap();
    read_error_until(&mut errors, "[approval] /approve 1").await;
    signal(&child, "-INT").await;
    let report = read_event_until(&mut events, |event| event["type"] == "run_end").await;
    assert_eq!(report["report"]["outcome"], "aborted");
    stdin.write_all(b"/approve 1 once\nsecond\n/quit\n").await.unwrap();
    drop(stdin);
    let report = read_event_until(&mut events, |event| event["type"] == "run_end").await;
    assert_eq!(report["report"]["finalText"], "safe");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .code(),
        Some(0)
    );
    assert!(!dir.path().join("marker").exists());
    assert_eq!(users(&server, 1), ["first", "second"]);
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_exits_even_when_the_input_reader_is_waiting_for_a_line() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![]).await;
    let mut child = chat(&server, dir.path(), &["--tools", ""]).spawn().unwrap();
    let _stdin = child.stdin.take().unwrap();
    let mut errors = BufReader::new(child.stderr.take().unwrap()).lines();
    read_error_until(&mut errors, "[chat] model=").await;
    signal(&child, "-TERM").await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), child.wait())
            .await
            .unwrap()
            .unwrap()
            .code(),
        Some(5)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn ctrl_c_while_idle_keeps_chat_open() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("done")]).await;
    let mut child = chat(&server, dir.path(), &["--tools", ""]).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut errors = BufReader::new(child.stderr.take().unwrap()).lines();
    read_error_until(&mut errors, "[chat] model=").await;
    signal(&child, "-INT").await;
    read_error_until(&mut errors, "no active task").await;
    stdin.write_all(b"hello\n/quit\n").await.unwrap();
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(stdout(&output), "done\n");
}

#[cfg(unix)]
fn terminal_input() -> (tokio::fs::File, std::os::fd::OwnedFd) {
    use std::os::fd::{FromRawFd, OwnedFd};
    let mut master = -1;
    let mut slave = -1;
    // SAFETY: both output pointers refer to initialized local integers, and null optional
    // name/termios/winsize pointers request the OS defaults. Success yields two fresh FDs.
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
    // SAFETY: openpty succeeded; each descriptor has exactly one owner from here on.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    (tokio::fs::File::from_std(std::fs::File::from(master)), slave)
}

#[cfg(unix)]
#[tokio::test]
async fn tty_input_steers_a_running_task_and_model_changes_wait_until_idle() {
    let dir = tempfile::tempdir().unwrap();
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let chunks = vec![json!({"choices":[{"index":0,"delta":{"content":"original"},"finish_reason":null}]})];
    let server = MockServer::start(vec![
        Reply::Gate {
            chunks,
            release: release.clone(),
        },
        Reply::text("steered"),
    ])
    .await;
    let (mut terminal, slave) = terminal_input();
    let mut child = chat(&server, dir.path(), &["--tools", "", "--output", "jsonl", "--deltas"])
        .stdin(Stdio::from(slave))
        .spawn()
        .unwrap();
    let mut events = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut errors = BufReader::new(child.stderr.take().unwrap()).lines();
    terminal.write_all(b"first\n").await.unwrap();
    terminal.flush().await.unwrap();
    read_event_until(&mut events, |event| event["type"] == "message_update").await;
    terminal.write_all(b"/model should-not-change\nsecond\n").await.unwrap();
    terminal.flush().await.unwrap();
    read_error_until(&mut errors, "before changing models").await;
    read_error_until(&mut errors, "steering input queued").await;
    release.notify_one();
    let report = read_event_until(&mut events, |event| event["type"] == "run_end").await;
    assert_eq!(report["report"]["finalText"], "steered");
    assert_eq!(report["report"]["turns"], 2);
    assert_eq!(users(&server, 1), ["first", "second"]);
    assert_eq!(server.requests()[1].body["model"], "mock-model");
    terminal.write_all(b"/quit\n").await.unwrap();
    terminal.flush().await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .code(),
        Some(0)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn new_while_a_tty_task_runs_cancels_it_and_starts_with_empty_history() {
    let dir = tempfile::tempdir().unwrap();
    let chunks = vec![json!({"choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":null}]})];
    let server = MockServer::start(vec![Reply::Stall(chunks), Reply::text("fresh")]).await;
    let (mut terminal, slave) = terminal_input();
    let mut child = chat(&server, dir.path(), &["--tools", "", "--output", "jsonl", "--deltas"])
        .stdin(Stdio::from(slave))
        .spawn()
        .unwrap();
    let mut events = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut errors = BufReader::new(child.stderr.take().unwrap()).lines();
    terminal.write_all(b"first\n").await.unwrap();
    terminal.flush().await.unwrap();
    read_event_until(&mut events, |event| event["type"] == "message_update").await;
    terminal.write_all(b"/new\n").await.unwrap();
    terminal.flush().await.unwrap();
    let report = read_event_until(&mut events, |event| event["type"] == "run_end").await;
    assert_eq!(report["report"]["outcome"], "aborted");
    read_error_until(&mut errors, "new conversation").await;
    terminal.write_all(b"second\n").await.unwrap();
    terminal.flush().await.unwrap();
    let report = read_event_until(&mut events, |event| event["type"] == "run_end").await;
    assert_eq!(report["report"]["finalText"], "fresh");
    assert_eq!(users(&server, 1), ["second"]);
    terminal.write_all(b"/quit\n").await.unwrap();
    terminal.flush().await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .code(),
        Some(0)
    );
}

#[tokio::test]
async fn an_approval_cancelled_by_the_run_timeout_does_not_authorize_a_later_call() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::bash("c", "printf no > marker"), Reply::text("safe")]).await;
    let mut child = chat(
        &server,
        dir.path(),
        &["--tools", "bash", "--timeout-secs", "1", "--output", "jsonl"],
    )
    .spawn()
    .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut events = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut errors = BufReader::new(child.stderr.take().unwrap()).lines();
    stdin.write_all(b"first\n").await.unwrap();
    read_error_until(&mut errors, "[approval] /approve 1").await;
    let report = read_event_until(&mut events, |event| event["type"] == "run_end").await;
    assert_eq!(report["report"]["outcome"], "timeout");
    stdin.write_all(b"/approve 1 once\nsecond\n/quit\n").await.unwrap();
    drop(stdin);
    let report = read_event_until(&mut events, |event| event["type"] == "run_end").await;
    assert_eq!(report["report"]["finalText"], "safe");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .code(),
        Some(0)
    );
    assert!(!dir.path().join("marker").exists());
    assert_eq!(users(&server, 1), ["first", "second"]);
}

#[cfg(unix)]
#[tokio::test]
async fn quit_during_a_tty_approval_cancels_the_task_and_exits() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::bash("c", "printf no > marker")]).await;
    let (mut terminal, slave) = terminal_input();
    let mut child = chat(&server, dir.path(), &["--tools", "bash", "--output", "jsonl"])
        .stdin(Stdio::from(slave))
        .spawn()
        .unwrap();
    let mut errors = BufReader::new(child.stderr.take().unwrap()).lines();
    terminal.write_all(b"first\n").await.unwrap();
    terminal.flush().await.unwrap();
    read_error_until(&mut errors, "[approval] /approve 1").await;
    terminal.write_all(b"/quit\n").await.unwrap();
    terminal.flush().await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .code(),
        Some(0)
    );
    assert!(!dir.path().join("marker").exists());
}
