//! Interactive frontend contracts against the local fake HTTP service; no paid APIs.
mod common;

use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdout, Command};

use common::mock_server::{MockServer, Reply};
use zx_agent::ai::{AssistantMessage, ContentBlock, Message, StopReason};
use zx_agent::session::SessionStore;

const KEY_VAR: &str = "ZX_CHAT_TEST_KEY";
const KEY: &str = "sk-chat-test-key";

fn chat(server: &MockServer, cwd: &Path, args: &[&str]) -> Command {
    chat_retrying(server, cwd, args, "0")
}

/// Like `chat`, with the given number of retries for transient errors.
fn chat_retrying(server: &MockServer, cwd: &Path, args: &[&str], max_retries: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zx-agent"));
    command
        .arg("chat")
        .args(["--model", "mock-model", "--base-url", &server.base_url])
        .args([
            "--api-key-env",
            KEY_VAR,
            "--max-retries",
            max_retries,
            "--idle-timeout-secs",
            "10",
        ])
        .args(args)
        .current_dir(cwd)
        // chat keeps sessions under the home directory by default; never the real one.
        .env("HOME", cwd.join(".home"))
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

async fn seed_context(dir: &Path, bytes: usize) -> String {
    let path = dir.join("chat.jsonl");
    let mut store = SessionStore::create(&path, common::header()).unwrap();
    let id = store.append_message(Message::user("x".repeat(bytes))).await.unwrap();
    store.append_message(Message::user("recent input")).await.unwrap();
    id
}

fn summary_json(id: &str) -> String {
    json!({"facts":[{"text":"Old observation","sourceEntryIds":[id]}],"inferences":[],"plans":["Continue"]}).to_string()
}

#[tokio::test]
async fn multiple_prompts_keep_history_and_stream_each_answer_once() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("first answer"), Reply::text("second answer")]).await;
    let output = run(&server, dir.path(), &["--tools", ""], "你好\nsecond\n/quit\n").await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "first answer\nsecond answer\n");
    assert_eq!(
        stderr(&output).matches("[turn] 1 request · ").count(),
        2,
        "{}",
        stderr(&output)
    );
    assert_eq!(users(&server, 1), ["你好", "second"]);
    let system = server.requests()[0].body["messages"][0]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(system.contains("working with a human"));
    assert!(!system.contains("No one will answer"));
}

#[tokio::test]
async fn new_conversations_keep_the_instructions_read_at_startup() {
    let dir = tempfile::tempdir().unwrap();
    let instructions = dir.path().join("instructions.md");
    std::fs::write(&instructions, "Startup instructions MARKER-ONE").unwrap();
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
    let mut child = chat(
        &server,
        dir.path(),
        &["--tools", "", "--system-prompt-file", instructions.to_str().unwrap()],
    )
    .spawn()
    .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"hello\n/new\nhello again\n/quit\n").await.unwrap();
    stdin.shutdown().await.unwrap();
    drop(stdin);
    server.wait_for_requests(1).await;
    // A pipe cannot be read twice; changing the file shows whether /new reads it again.
    std::fs::write(&instructions, "Changed instructions MARKER-TWO").unwrap();
    release.notify_one();
    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stderr(&output).contains("new conversation"), "{}", stderr(&output));
    let system = |index: usize| {
        server.requests()[index].body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert!(system(0).contains("MARKER-ONE"));
    assert!(system(1).contains("MARKER-ONE"), "{}", system(1));
    assert!(!system(1).contains("MARKER-TWO"));
}

#[tokio::test]
async fn slash_commands_are_local_and_compact_without_history_is_a_noop() {
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
    assert!(stderr(&output).contains("no eligible old messages"));
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
async fn manual_compact_persists_a_summary_and_the_next_prompt_uses_it() {
    let dir = tempfile::tempdir().unwrap();
    let id = seed_context(dir.path(), 10_000).await;
    std::fs::write(dir.path().join("profile.toml"), "[compaction]\nkeepRecentBytes = 1\n").unwrap();
    let server = MockServer::start(vec![Reply::text(&summary_json(&id)), Reply::text("answer")]).await;
    let output = run(
        &server,
        dir.path(),
        &[
            "--tools",
            "",
            "--profile",
            "profile.toml",
            "--session",
            "chat.jsonl",
            "--output",
            "jsonl",
        ],
        "/compact\ncontinue\n/quit\n",
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let events: Vec<Value> = stdout(&output)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let compact = events
        .iter()
        .find(|event| event["type"] == "compaction_report")
        .unwrap();
    assert_eq!(compact["report"]["outcome"], "completed");
    assert_eq!(compact["report"]["requests"], 1);
    assert_eq!(compact["report"]["usage"]["totalTokens"], 12);
    let task = events.iter().find(|event| event["type"] == "run_end").unwrap();
    assert_eq!(task["report"]["finalText"], "answer");
    assert_eq!(task["report"]["modelRequests"], 1);
    assert!(task["report"].get("turns").is_none());
    assert!(users(&server, 1)[0].contains("<context_summary>"));
    assert!(
        !server.requests()[1].body["messages"]
            .to_string()
            .contains(&"x".repeat(1000))
    );
    assert_eq!(server.requests()[0].body["max_tokens"], 2048);
    assert!(server.requests()[0].body.get("tools").is_none());
    let store = SessionStore::open(&dir.path().join("chat.jsonl")).unwrap();
    assert!(store.latest_compaction().is_some());
    assert!(store.raw_projection().iter().any(|item| item.entry_id == id));
}

#[tokio::test]
async fn chat_automatically_compacts_at_its_default_byte_budget() {
    let dir = tempfile::tempdir().unwrap();
    let id = seed_context(dir.path(), 300 * 1024).await;
    let server = MockServer::start(vec![Reply::text(&summary_json(&id)), Reply::text("answer")]).await;
    let output = run(
        &server,
        dir.path(),
        &["--tools", "", "--session", "chat.jsonl", "--output", "jsonl"],
        "continue\n/quit\n",
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let events: Vec<Value> = stdout(&output)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let start = events.iter().find(|event| event["type"] == "compaction_start").unwrap();
    assert_eq!(start["automatic"], true);
    let report = events.iter().find(|event| event["type"] == "run_end").unwrap();
    assert_eq!(report["report"]["outcome"], "completed");
    assert_eq!(report["report"]["modelRequests"], 2);
    assert_eq!(report["report"]["usage"]["totalTokens"], 24);
    assert!(users(&server, 1)[0].contains("<context_summary>"));
}

#[tokio::test]
async fn explicit_cli_flags_disable_automatic_compaction() {
    for disable in [vec!["--no-auto-compact"], vec!["--compact-threshold-bytes", "0"]] {
        let dir = tempfile::tempdir().unwrap();
        seed_context(dir.path(), 300 * 1024).await;
        let server = MockServer::start(vec![Reply::text("answer")]).await;
        let mut args = vec!["--tools", "", "--session", "chat.jsonl"];
        args.extend(disable);
        let output = run(&server, dir.path(), &args, "continue\n/quit\n").await;
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        assert_eq!(server.requests().len(), 1);
        assert!(users(&server, 0)[0].len() > 256 * 1024);
        assert!(
            SessionStore::open(&dir.path().join("chat.jsonl"))
                .unwrap()
                .latest_compaction()
                .is_none()
        );
    }
}

#[tokio::test]
async fn a_failed_manual_summary_is_reported_and_chat_can_accept_another_task() {
    let dir = tempfile::tempdir().unwrap();
    seed_context(dir.path(), 10_000).await;
    std::fs::write(dir.path().join("profile.toml"), "[compaction]\nkeepRecentBytes = 1\n").unwrap();
    let server = MockServer::start(vec![Reply::text("invalid summary"), Reply::text("recovered")]).await;
    let output = run(
        &server,
        dir.path(),
        &[
            "--tools",
            "",
            "--profile",
            "profile.toml",
            "--session",
            "chat.jsonl",
            "--output",
            "jsonl",
        ],
        "/compact\ncontinue\n/quit\n",
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let events: Vec<Value> = stdout(&output)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        events
            .iter()
            .find(|event| event["type"] == "compaction_report")
            .unwrap()["report"]["outcome"],
        "failed"
    );
    assert_eq!(
        events.iter().find(|event| event["type"] == "run_end").unwrap()["report"]["finalText"],
        "recovered"
    );
    assert!(
        !server.requests()[1].body["messages"]
            .to_string()
            .contains("invalid summary")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sigint_cancels_a_manual_summary_and_keeps_the_conversation_reusable() {
    let dir = tempfile::tempdir().unwrap();
    seed_context(dir.path(), 10_000).await;
    std::fs::write(dir.path().join("profile.toml"), "[compaction]\nkeepRecentBytes = 1\n").unwrap();
    let server = MockServer::start(vec![Reply::Stall(vec![]), Reply::text("reused")]).await;
    let mut child = chat(
        &server,
        dir.path(),
        &[
            "--tools",
            "",
            "--profile",
            "profile.toml",
            "--session",
            "chat.jsonl",
            "--output",
            "jsonl",
        ],
    )
    .spawn()
    .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut events = BufReader::new(child.stdout.take().unwrap()).lines();
    stdin.write_all(b"/compact\n").await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), server.wait_for_requests(1))
        .await
        .unwrap();
    signal(&child, "-INT").await;
    let compact = read_event_until(&mut events, |event| event["type"] == "compaction_report").await;
    assert_eq!(compact["report"]["outcome"], "aborted");
    stdin.write_all(b"continue\n/quit\n").await.unwrap();
    drop(stdin);
    let task = read_event_until(&mut events, |event| event["type"] == "run_end").await;
    assert_eq!(task["report"]["finalText"], "reused");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .code(),
        Some(0)
    );
    assert!(
        SessionStore::open(&dir.path().join("chat.jsonl"))
            .unwrap()
            .latest_compaction()
            .is_none()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn tty_compaction_rejects_overlaps_and_runs_one_queued_input_after_completion() {
    let dir = tempfile::tempdir().unwrap();
    let id = seed_context(dir.path(), 10_000).await;
    std::fs::write(dir.path().join("profile.toml"), "[compaction]\nkeepRecentBytes = 1\n").unwrap();
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let chunks = vec![json!({"choices":[{"index":0,"delta":{"content":summary_json(&id)},"finish_reason":null}]})];
    let server = MockServer::start(vec![
        Reply::Gate {
            chunks,
            release: release.clone(),
        },
        Reply::text("queued answer"),
    ])
    .await;
    let (mut terminal, slave) = common::pty();
    let mut child = chat(
        &server,
        dir.path(),
        &[
            "--tools",
            "",
            "--profile",
            "profile.toml",
            "--session",
            "chat.jsonl",
            "--output",
            "jsonl",
        ],
    )
    .stdin(Stdio::from(slave))
    .spawn()
    .unwrap();
    let mut events = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut errors = BufReader::new(child.stderr.take().unwrap()).lines();
    terminal.write_all(b"/compact\n").await.unwrap();
    terminal.flush().await.unwrap();
    read_event_until(&mut events, |event| event["type"] == "compaction_start").await;
    terminal
        .write_all(b"/compact\n/model no-switch\nqueued task\n")
        .await
        .unwrap();
    terminal.flush().await.unwrap();
    read_error_until(&mut errors, "session is busy").await;
    read_error_until(&mut errors, "before changing models").await;
    read_error_until(&mut errors, "input queued after compaction").await;
    release.notify_one();
    let compact = read_event_until(&mut events, |event| event["type"] == "compaction_report").await;
    assert_eq!(compact["report"]["outcome"], "completed");
    let task = read_event_until(&mut events, |event| event["type"] == "run_end").await;
    assert_eq!(task["report"]["finalText"], "queued answer");
    assert_eq!(server.requests()[1].body["model"], "mock-model");
    assert_eq!(users(&server, 1).last().unwrap(), "queued task");
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
async fn reasoning_command_changes_subsequent_requests() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("one"), Reply::text("two"), Reply::text("three")]).await;
    let output = run(
        &server,
        dir.path(),
        &["--tools", "", "--reasoning", "low"],
        "first\n/reasoning high\nsecond\n/reasoning bogus\n/reasoning default\nthird\n/reasoning\n/quit\n",
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let efforts: Vec<Value> = server
        .requests()
        .iter()
        .map(|request| request.body.get("reasoning_effort").cloned().unwrap_or(Value::Null))
        .collect();
    assert_eq!(efforts, [json!("low"), json!("high"), Value::Null]);
    let errors = stderr(&output);
    assert!(errors.contains("unknown reasoning level \"bogus\""), "{errors}");
    assert!(errors.contains("[chat] reasoning=default"), "{errors}");
}

#[tokio::test]
async fn continue_command_answers_again_after_a_failed_answer() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![
        Reply::Status(400, r#"{"error":{"message":"bad request"}}"#),
        Reply::text("second try"),
    ])
    .await;
    let output = run(
        &server,
        dir.path(),
        &["--tools", ""],
        "first\n/continue\n/continue\n/quit\n",
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(server.requests().len(), 2);
    assert_eq!(users(&server, 1), ["first"]);
    assert!(stdout(&output).contains("second try"), "{}", stdout(&output));
    // The second /continue finds a finished answer and makes no request.
    assert!(
        stderr(&output).contains("ends with a finished answer"),
        "{}",
        stderr(&output)
    );
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
    let output = run(&server, dir.path(), &["--tools", "bash"], "go\n1\n/quit\n").await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(std::fs::read_to_string(dir.path().join("marker")).unwrap(), "approved");
    assert!(stderr(&output).contains("[approval 1] bash: printf approved > marker"));
    assert!(stderr(&output).contains("  profile requires tool approval"));
    // The scope and the auto-approved commands are stated once, at startup.
    assert!(
        stderr(&output).contains(
            "[chat] tools=bash · bash asks first, except grep, head, tail, wc, jq · commands run with this process's permissions · /help for commands"
        ),
        "{}",
        stderr(&output)
    );
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn an_auto_approved_command_is_shown_with_a_preview_of_its_result() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::bash("c", "seq 1 5"), Reply::text("done")]).await;
    let output = run(
        &server,
        dir.path(),
        &["--tools", "bash", "--auto-approve-cmd", "seq"],
        "go\n/quit\n",
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let log = stderr(&output);
    assert!(!log.contains("[approval"), "{log}");
    assert!(log.contains("[tool] bash: seq 1 5 -> ok"), "{log}");
    // Three lines of the result, then the count of the rest (the exit code line included).
    assert!(log.contains("\n    1\n    2\n    3\n    … 3 more line(s)\n"), "{log}");
}

#[tokio::test]
async fn y_and_n_answer_an_approval_like_1_and_3() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![
        Reply::bash("c1", "printf yes > first"),
        Reply::bash("c2", "printf no > second"),
        Reply::text("done"),
    ])
    .await;
    let output = run(&server, dir.path(), &["--tools", "bash"], "go\ny\nN\n/quit\n").await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(std::fs::read_to_string(dir.path().join("first")).unwrap(), "yes");
    assert!(!dir.path().join("second").exists());
    assert_eq!(server.requests().len(), 3);
    assert!(
        server.requests()[2].body["messages"]
            .to_string()
            .contains("declined by the user")
    );
}

#[tokio::test]
async fn status_shows_the_session_and_what_the_conversation_used() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("hi")]).await;
    let output = run(
        &server,
        dir.path(),
        &["--tools", "", "--session", "chat.jsonl"],
        "hello\n/status\n/quit\n",
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let log = stderr(&output);
    assert!(
        log.contains("[status] model=mock-model · profile=assistant · session=chat.jsonl · cwd="),
        "{log}"
    );
    assert!(
        log.contains("[status] this conversation: 1 turn(s) · 1 request(s) · "),
        "{log}"
    );
}

#[tokio::test]
async fn a_file_write_approval_shows_the_content_as_lines() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![
        Reply::tool_call("c", "write_file", json!({"path": "notes.txt", "content": "one\ntwo\n"})),
        Reply::text("done"),
    ])
    .await;
    let output = run(&server, dir.path(), &[], "go\n1\n/quit\n").await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let log = stderr(&output);
    assert!(
        log.contains("[approval 1] write_file: notes.txt\n  content (2 lines):\n      one\n      two\n"),
        "{log}"
    );
    assert!(log.contains("[tool] write_file: notes.txt -> ok"), "{log}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("notes.txt")).unwrap(),
        "one\ntwo\n"
    );
}

#[tokio::test]
async fn an_edit_approval_lists_each_edit_and_its_result_shows_the_patch() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello\nworld\n").unwrap();
    let server = MockServer::start(vec![
        Reply::tool_call(
            "c",
            "edit_file",
            json!({"path": "a.txt", "edits": [{"oldText": "hello", "newText": "bye"}]}),
        ),
        Reply::text("done"),
    ])
    .await;
    let output = run(&server, dir.path(), &[], "go\n1\n/quit\n").await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let log = stderr(&output);
    assert!(
        log.contains("[approval 1] edit_file: a.txt\n  edits[0].oldText: hello\n  edits[0].newText: bye\n"),
        "{log}"
    );
    assert!(log.contains("[tool] edit_file: a.txt -> ok"), "{log}");
    assert!(log.contains("\n    -hello\n    +bye\n"), "{log}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "bye\nworld\n"
    );
}

#[tokio::test]
async fn color_always_keeps_colors_when_stderr_is_not_a_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::bash("c", "seq 1 2"), Reply::text("done")]).await;
    let output = run(
        &server,
        dir.path(),
        &["--tools", "bash", "--auto-approve-cmd", "seq", "--color", "always"],
        "go\n/quit\n",
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let log = stderr(&output);
    assert!(
        log.contains("\x1b[2m[tool]\x1b[0m bash: seq 1 2 -> \x1b[32mok\x1b[0m"),
        "{log}"
    );
    assert!(log.contains("\x1b[2m[turn] "), "{log}");
}

#[tokio::test]
async fn resuming_a_session_file_says_how_much_history_it_holds() {
    let dir = tempfile::tempdir().unwrap();
    seed_context(dir.path(), 10).await;
    let server = MockServer::start(vec![Reply::text("hi")]).await;
    let output = run(
        &server,
        dir.path(),
        &["--tools", "", "--session", "chat.jsonl"],
        "hello\n/quit\n",
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let log = stderr(&output);
    assert!(
        log.contains("[chat] resumed 2 message(s) from the session file; last activity 20"),
        "{log}"
    );

    let fresh = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("hi")]).await;
    let output = run(
        &server,
        fresh.path(),
        &["--tools", "", "--session", "new.jsonl"],
        "hello\n/quit\n",
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!stderr(&output).contains("resumed"), "{}", stderr(&output));
}

#[tokio::test]
async fn a_retried_error_is_named_once() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![
        Reply::Status(500, r#"{"error":{"message":"upstream exploded"}}"#),
        Reply::text("recovered"),
    ])
    .await;
    let mut child = chat_retrying(&server, dir.path(), &["--tools", ""], "1")
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"go\n/quit\n").await.unwrap();
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(15), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "recovered\n");
    let log = stderr(&output);
    assert_eq!(log.matches("upstream exploded").count(), 1, "{log}");
    assert!(log.contains("[retry] 1/1 in "), "{log}");
    assert!(log.contains("ms (same error)\n"), "{log}");
    assert!(log.contains("[turn] 2 requests · 1 retry · "), "{log}");
}

#[tokio::test]
async fn a_turn_without_reported_usage_omits_the_token_counts() {
    let dir = tempfile::tempdir().unwrap();
    let answer = json!({"choices":[{"index":0,"delta":{"content":"no usage"},"finish_reason":"stop"}]});
    let server = MockServer::start(vec![Reply::Sse(vec![answer])]).await;
    let output = run(&server, dir.path(), &["--tools", ""], "go\n/quit\n").await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let log = stderr(&output);
    assert!(log.contains("[turn] 1 request · "), "{log}");
    assert!(!log.contains(" in · "), "{log}");
}

#[tokio::test]
async fn show_thinking_streams_reasoning_on_stderr_only_when_asked() {
    let reply = || {
        Reply::Sse(vec![
            json!({"choices":[{"index":0,"delta":{"reasoning_content":"让我想想"},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{"content":"答案"},"finish_reason":"stop"}]}),
        ])
    };
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![reply()]).await;
    let output = run(&server, dir.path(), &["--tools", ""], "go\n/quit\n").await;
    assert_eq!(stdout(&output), "答案\n");
    assert!(!stderr(&output).contains("让我想想"), "{}", stderr(&output));

    let server = MockServer::start(vec![reply()]).await;
    let output = run(&server, dir.path(), &["--tools", "", "--show-thinking"], "go\n/quit\n").await;
    assert_eq!(stdout(&output), "答案\n");
    assert!(stderr(&output).contains("让我想想\n"), "{}", stderr(&output));
}

#[cfg(unix)]
#[tokio::test]
async fn ctrl_c_twice_while_idle_exits() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![]).await;
    let mut child = chat(&server, dir.path(), &["--tools", ""]).spawn().unwrap();
    // Kept open: closing stdin would end the conversation by itself.
    let _stdin = child.stdin.take().unwrap();
    let mut errors = BufReader::new(child.stderr.take().unwrap()).lines();
    read_error_until(&mut errors, "/help for commands").await;
    signal(&child, "-INT").await;
    read_error_until(&mut errors, "press Ctrl+C again").await;
    signal(&child, "-INT").await;
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
async fn a_shell_call_waits_for_an_approval_before_any_side_effect() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::bash("c", "printf approved > marker"), Reply::text("done")]).await;
    let mut child = chat(&server, dir.path(), &["--tools", "bash"]).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut errors = BufReader::new(child.stderr.take().unwrap()).lines();
    stdin.write_all(b"go\n").await.unwrap();
    read_error_until(&mut errors, "[approval] Choose 1, 2, 3 or 4:").await;
    assert!(!dir.path().join("marker").exists());
    stdin.write_all(b"1\n/quit\n").await.unwrap();
    drop(stdin);
    let result = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status.code(), Some(0));
    assert!(dir.path().join("marker").exists());
}

#[tokio::test]
async fn a_file_write_waits_for_an_approval_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![
        Reply::tool_call("c", "write_file", json!({"path": "notes.txt", "content": "hello\n"})),
        Reply::text("done"),
    ])
    .await;
    // The default assistant profile enables write_file and asks before it runs.
    let mut child = chat(&server, dir.path(), &[]).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut errors = BufReader::new(child.stderr.take().unwrap()).lines();
    stdin.write_all(b"go\n").await.unwrap();
    read_error_until(&mut errors, "[approval] Choose 1, 2, 3 or 4:").await;
    assert!(!dir.path().join("notes.txt").exists());
    stdin.write_all(b"1\n/quit\n").await.unwrap();
    drop(stdin);
    let result = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("notes.txt")).unwrap(),
        "hello\n"
    );
}

#[tokio::test]
async fn discussing_an_approval_sends_user_input_without_executing_the_call() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::bash("c", "printf no > marker"), Reply::text("explained")]).await;
    let output = run(
        &server,
        dir.path(),
        &["--tools", "bash"],
        "go\n4\nWhy is this needed?\n/quit\n",
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!dir.path().join("marker").exists());
    assert!(stderr(&output).contains("4 discuss"));
    assert!(stderr(&output).contains("What would you like to discuss?"));
    assert!(
        server.requests()[1].body["messages"]
            .to_string()
            .contains("Why is this needed?")
    );
    assert!(
        server.requests()[1].body["messages"]
            .to_string()
            .contains("user wants to discuss")
    );
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
    let output = run(&server, dir.path(), &["--tools", "bash"], "go\n2\n/quit\n").await;
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
    let output = run(&server, dir.path(), &["--tools", "bash"], "go\n2\n3\n/quit\n").await;
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
    let output = run(&server, dir.path(), &["--tools", "bash"], "go\n3\n/quit\n").await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!dir.path().join("marker").exists());
    let body = server.requests()[1].body["messages"].to_string();
    assert!(body.contains("declined by the user"));
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
        "go\n/quit\n",
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
async fn listed_read_only_commands_run_without_approval() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("app.log"), "ok\nerror: disk full\n").unwrap();
    let server = MockServer::start(vec![
        Reply::bash("c", "grep error app.log | head -n 1"),
        Reply::text("done"),
    ])
    .await;
    let output = run(&server, dir.path(), &["--tools", "bash"], "go\n/quit\n").await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!stderr(&output).contains("[approval"), "{}", stderr(&output));
    assert!(
        server.requests()[1].body["messages"]
            .to_string()
            .contains("error: disk full")
    );
}

#[tokio::test]
async fn listed_commands_still_ask_when_they_mention_secrets_or_redirect() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("app.log"), "line\n").unwrap();
    let server = MockServer::start(vec![
        Reply::bash("c1", "grep secret app.log"),
        Reply::bash("c2", "head -n 1 app.log > copy"),
        Reply::text("done"),
    ])
    .await;
    let output = run(&server, dir.path(), &["--tools", "bash"], "go\n3\n3\n/quit\n").await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stderr(&output).contains("[approval 1]"));
    assert!(stderr(&output).contains("[approval 2]"));
    assert!(!dir.path().join("copy").exists());
}

#[tokio::test]
async fn the_auto_approve_flag_replaces_the_profile_list() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("app.log"), "line\n").unwrap();
    let server = MockServer::start(vec![
        Reply::bash("c1", "printf flagged"),
        Reply::bash("c2", "head -n 1 app.log"),
        Reply::text("done"),
    ])
    .await;
    let output = run(
        &server,
        dir.path(),
        &["--tools", "bash", "--auto-approve-cmd", "printf"],
        "go\n3\n/quit\n",
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(server.requests()[1].body["messages"].to_string().contains("flagged"));
    // head is no longer listed, so it is the run's first approval request.
    assert!(stderr(&output).contains("[approval 1]"));
    assert!(stderr(&output).contains("head -n 1 app.log"));
    assert!(!stderr(&output).contains("[approval 2]"));
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
async fn invalid_approval_choices_do_not_authorize_the_current_call() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::bash("c", "printf no > marker"), Reply::text("handled")]).await;
    let output = run(&server, dir.path(), &["--tools", "bash"], "go\n9\n3\n/quit\n").await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!dir.path().join("marker").exists());
    assert!(stderr(&output).contains("choose 1, 2, 3 or 4"));
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
    let input = "first\n2\n/new\nsecond\n3\n/quit\n";
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
        &["--tools", "", "--max-model-requests", "0", "--output", "jsonl"],
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
    assert_eq!(reports[0]["report"]["outcome"], "model_request_limit");
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

#[cfg(unix)]
#[tokio::test]
async fn debug_starts_on_a_new_line_and_respects_no_color() {
    for no_color in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start(vec![Reply::text("last assistant line")]).await;
        let (mut terminal, slave) = common::pty();
        let mut command = chat(&server, dir.path(), &["--tools", "", "--debug"]);
        command
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        command.env("TERM", "xterm").env("TZ", "Asia/Shanghai");
        if no_color {
            command.env("NO_COLOR", "1");
        } else {
            command.env_remove("NO_COLOR");
        }
        let mut child = command.spawn().unwrap();
        drop(command);
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"go\n/quit\n").await.unwrap();
        drop(stdin);
        let mut bytes = Vec::new();
        let reader = tokio::spawn(async move {
            let mut chunk = [0; 4096];
            while let Ok(n) = terminal.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..n]);
            }
            bytes
        });
        let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(status.success());
        let output = String::from_utf8(reader.await.unwrap()).unwrap();
        let after = output.split_once("last assistant line").expect(&output).1;
        assert!(after.starts_with("\r\n") || after.starts_with('\n'), "{output:?}");
        assert!(after.contains("Response #1"), "{output:?}");
        assert!(output.contains("+08:00]"), "{output:?}");
        if no_color {
            assert!(!output.contains("\x1b[36m"), "{output:?}");
        } else {
            assert!(output.contains("\x1b[36m[debug "), "{output:?}");
        }
    }
}

#[tokio::test]
async fn debug_shows_execution_events_and_filtered_tool_results() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("credentials.txt"), format!("prefix {KEY} suffix")).unwrap();
    let tool = json!({"choices":[{"index":0,"delta":{"tool_calls":[{
        "index":0,"id":"read","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"credentials.txt\"}"}
    }]},"finish_reason":"tool_calls"}]});
    let server = MockServer::start(vec![Reply::Sse(vec![tool]), Reply::text("done")]).await;
    let output = run(
        &server,
        dir.path(),
        &["--tools", "read_file", "--debug"],
        "read\n/quit\n",
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let debug = stderr(&output);
    for expected in [
        "Request #1 · Task",
        "turn=1 · attempt=1",
        "Request #2 · Task",
        "turn=2 · attempt=1",
        "Tool call · read_file",
        "Tool policy · read_file",
        "Tool finished · read_file",
        "Tool result · read_file",
        "credentials.txt",
        "[REDACTED]",
        "Response #2",
        "Task finished",
        "tokens in=",
    ] {
        assert!(debug.contains(expected), "missing {expected}: {debug}");
    }
    assert!(!debug.contains(KEY), "{debug}");
    assert_eq!(stdout(&output), "done\n");
}

#[tokio::test]
async fn debug_shows_policy_approval_and_denied_tool_result() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::bash("c", "printf forbidden > marker"), Reply::text("done")]).await;
    let output = run(&server, dir.path(), &["--tools", "bash", "--debug"], "go\n3\n/quit\n").await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!dir.path().join("marker").exists());
    let log = stderr(&output);
    for expected in [
        "Tool call · bash",
        "Tool policy · bash",
        "Approval requested · bash",
        "Approval resolved · bash",
        "Denied",
        "not executed / duration unavailable",
        "Tool result · bash",
        "Task finished",
    ] {
        assert!(log.contains(expected), "missing {expected}: {log}");
    }
}

#[tokio::test]
async fn debug_reports_model_failure_without_repeating_the_error() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::Status(400, "provider-failure-marker")]).await;
    let output = run(&server, dir.path(), &["--tools", "", "--debug"], "go\n/quit\n").await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let log = stderr(&output);
    assert!(log.contains("Request #1 · Task"), "{log}");
    assert!(log.contains("Response #1 · Error · elapsed="), "{log}");
    assert!(log.contains("Task finished · Error"), "{log}");
    assert_eq!(log.matches("provider-failure-marker").count(), 1, "{log}");
}

#[tokio::test]
async fn an_approved_command_is_shown_with_the_request_and_again_with_its_result() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::bash("c", "printf example > marker"), Reply::text("done")]).await;
    let output = run(&server, dir.path(), &["--tools", "bash"], "go\n1\n/quit\n").await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let log = stderr(&output);
    assert_eq!(log.matches("printf example > marker").count(), 2, "{log}");
    assert!(log.contains("[approval 1] bash: printf example > marker"), "{log}");
    assert!(log.contains("[tool] bash: printf example > marker -> ok"), "{log}");
    assert!(!log.contains("[debug "));
}

#[tokio::test]
async fn debug_does_not_change_jsonl_events() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("done")]).await;
    let output = run(
        &server,
        dir.path(),
        &["--tools", "", "--debug", "--output", "jsonl"],
        "go\n/quit\n",
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!stderr(&output).contains("[debug "));
    for line in stdout(&output).lines() {
        serde_json::from_str::<Value>(line).unwrap();
    }
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

/// Read the terminal until `needle` has been seen, keeping everything read in `seen`.
async fn read_terminal_until(terminal: &mut tokio::fs::File, seen: &mut Vec<u8>, needle: &str) {
    tokio::time::timeout(Duration::from_secs(8), async {
        let mut chunk = [0; 4096];
        while !String::from_utf8_lossy(seen).contains(needle) {
            let n = terminal.read(&mut chunk).await.unwrap();
            assert!(n > 0, "terminal closed before {needle:?}");
            seen.extend_from_slice(&chunk[..n]);
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{needle:?} not seen in {:?}", String::from_utf8_lossy(seen)));
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
async fn a_cancelled_task_is_reported_once_without_a_model_error_line() {
    let dir = tempfile::tempdir().unwrap();
    let partial = json!({"choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":null}]});
    let server = MockServer::start(vec![Reply::Stall(vec![partial])]).await;
    let mut child = chat(&server, dir.path(), &["--tools", ""]).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    stdin.write_all(b"first\n").await.unwrap();
    let mut seen = Vec::new();
    tokio::time::timeout(Duration::from_secs(8), async {
        let mut chunk = [0; 256];
        while !String::from_utf8_lossy(&seen).contains("partial") {
            let n = stdout.read(&mut chunk).await.unwrap();
            assert!(n > 0, "stdout ended early");
            seen.extend_from_slice(&chunk[..n]);
        }
    })
    .await
    .unwrap();
    signal(&child, "-INT").await;
    stdin.write_all(b"/quit\n").await.unwrap();
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let log = stderr(&output);
    assert!(log.contains("[chat] run was cancelled"), "{log}");
    assert!(!log.contains("[model] Aborted"), "{log}");
    assert!(log.contains("[turn] 1 request · "), "{log}");
}

#[cfg(unix)]
#[tokio::test]
async fn lines_pasted_together_into_a_terminal_are_one_message() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("got it")]).await;
    let (mut terminal, slave) = common::pty();
    let mut child = chat(&server, dir.path(), &["--tools", "", "--output", "jsonl"])
        .stdin(Stdio::from(slave))
        .spawn()
        .unwrap();
    let mut events = BufReader::new(child.stdout.take().unwrap()).lines();
    terminal
        .write_all("第一行：日志\n第二行：ERROR\n请分析\n".as_bytes())
        .await
        .unwrap();
    terminal.flush().await.unwrap();
    let report = read_event_until(&mut events, |event| event["type"] == "run_end").await;
    assert_eq!(report["report"]["finalText"], "got it");
    assert_eq!(users(&server, 0), ["第一行：日志\n第二行：ERROR\n请分析"]);
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
async fn a_terminal_shows_a_status_line_while_the_model_is_silent() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::Stall(vec![])]).await;
    let (mut terminal, slave) = common::pty();
    let mut command = chat(&server, dir.path(), &["--tools", ""]);
    command
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    command.env("TERM", "xterm").env_remove("NO_COLOR");
    let mut child = command.spawn().unwrap();
    drop(command);
    terminal.write_all(b"go\n").await.unwrap();
    terminal.flush().await.unwrap();
    let mut seen = Vec::new();
    read_terminal_until(&mut terminal, &mut seen, "waiting for the model · ").await;
    let output = String::from_utf8_lossy(&seen).into_owned();
    assert!(output.contains("\x1b[2m"), "the status is dimmed: {output:?}");
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
async fn a_terminal_separates_answers_with_blank_lines() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("answer")]).await;
    let (mut terminal, slave) = common::pty();
    let mut command = chat(&server, dir.path(), &["--tools", ""]);
    command
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    command.arg("--color").arg("never");
    command.env("TERM", "xterm").env_remove("NO_COLOR");
    let mut child = command.spawn().unwrap();
    drop(command);
    let mut seen = Vec::new();
    // Typed after the prompt, as a person would, so the echo lands below it.
    read_terminal_until(&mut terminal, &mut seen, "zx> ").await;
    terminal.write_all(b"go\n").await.unwrap();
    terminal.flush().await.unwrap();
    read_terminal_until(&mut terminal, &mut seen, "[turn]").await;
    let output = String::from_utf8_lossy(&seen).replace('\r', "");
    assert!(output.contains("zx> go\n\nanswer\n\n[turn] 1 request"), "{output:?}");
    assert!(!output.contains("\x1b["), "{output:?}");
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
    read_error_until(&mut errors, "[approval] Choose 1, 2, 3 or 4:").await;
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
    let (mut terminal, slave) = common::pty();
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
    assert_eq!(report["report"]["modelRequests"], 2);
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
async fn input_steered_into_a_cancelled_task_is_discarded_not_sent_later() {
    let dir = tempfile::tempdir().unwrap();
    let chunks = vec![json!({"choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":null}]})];
    let server = MockServer::start(vec![Reply::Stall(chunks), Reply::text("fresh")]).await;
    let (mut terminal, slave) = common::pty();
    let mut child = chat(&server, dir.path(), &["--tools", "", "--output", "jsonl", "--deltas"])
        .stdin(Stdio::from(slave))
        .spawn()
        .unwrap();
    let mut events = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut errors = BufReader::new(child.stderr.take().unwrap()).lines();
    terminal.write_all(b"first\n").await.unwrap();
    terminal.flush().await.unwrap();
    read_event_until(&mut events, |event| event["type"] == "message_update").await;
    terminal.write_all(b"late steering\n").await.unwrap();
    terminal.flush().await.unwrap();
    read_error_until(&mut errors, "steering input queued").await;
    signal(&child, "-INT").await;
    let report = read_event_until(&mut events, |event| event["type"] == "run_end").await;
    assert_eq!(report["report"]["outcome"], "aborted");
    terminal.write_all(b"second\n").await.unwrap();
    terminal.flush().await.unwrap();
    let report = read_event_until(&mut events, |event| event["type"] == "run_end").await;
    assert_eq!(report["report"]["finalText"], "fresh");
    assert_eq!(users(&server, 1), ["first", "second"]);
    read_error_until(&mut errors, "discarded").await;
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
async fn an_expired_approval_frees_the_next_scripted_line() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![
        Reply::bash("c", "printf no > marker"),
        Reply::text("went on without it"),
        Reply::text("second answer"),
    ])
    .await;
    let mut child = chat(
        &server,
        dir.path(),
        &["--tools", "bash", "--approval-timeout-secs", "1", "--output", "jsonl"],
    )
    .spawn()
    .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut events = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut errors = BufReader::new(child.stderr.take().unwrap()).lines();
    stdin.write_all(b"first\n").await.unwrap();
    read_error_until(&mut errors, "[approval] Choose 1, 2, 3 or 4:").await;
    let resolved = read_event_until(&mut events, |event| event["type"] == "approval_resolved").await;
    assert_eq!(resolved["approval"], "timed_out");
    // The task goes on without the call, so a line written now is the next question.
    stdin.write_all(b"second\n/quit\n").await.unwrap();
    drop(stdin);
    read_event_until(&mut events, |event| event["type"] == "run_end").await;
    let report = read_event_until(&mut events, |event| event["type"] == "run_end").await;
    assert_eq!(report["report"]["finalText"], "second answer");
    assert_eq!(users(&server, 2), ["first", "second"]);
    read_error_until(&mut errors, "expired").await;
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

#[cfg(unix)]
#[tokio::test]
async fn new_while_a_tty_task_runs_cancels_it_and_starts_with_empty_history() {
    let dir = tempfile::tempdir().unwrap();
    let chunks = vec![json!({"choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":null}]})];
    let server = MockServer::start(vec![Reply::Stall(chunks), Reply::text("fresh")]).await;
    let (mut terminal, slave) = common::pty();
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
    read_error_until(&mut errors, "[approval] Choose 1, 2, 3 or 4:").await;
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
    let (mut terminal, slave) = common::pty();
    let mut child = chat(&server, dir.path(), &["--tools", "bash", "--output", "jsonl"])
        .stdin(Stdio::from(slave))
        .spawn()
        .unwrap();
    let mut errors = BufReader::new(child.stderr.take().unwrap()).lines();
    terminal.write_all(b"first\n").await.unwrap();
    terminal.flush().await.unwrap();
    read_error_until(&mut errors, "[approval] Choose 1, 2, 3 or 4:").await;
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

#[tokio::test]
async fn resuming_an_interrupted_session_reports_the_recovered_call_without_running_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = SessionStore::create(&dir.path().join("chat.jsonl"), common::header()).unwrap();
    store.append_message(Message::user("fix it")).await.unwrap();
    let mut call = AssistantMessage::failure("mock", "mock-model", StopReason::ToolUse, "");
    call.error_message = None;
    call.content = vec![ContentBlock::ToolCall {
        id: "c1".into(),
        name: "bash".into(),
        arguments: json!({"command": "touch ran-again"}),
        arguments_error: None,
    }];
    store.append_message(Message::Assistant(call)).await.unwrap();
    store.append_tool_call_started("c1", "bash").await.unwrap();
    drop(store);
    let server = MockServer::start(vec![Reply::text("checked")]).await;

    let output = run(
        &server,
        dir.path(),
        &["--tools", "bash", "--session", "chat.jsonl"],
        "continue\n/quit\n",
    )
    .await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("[recover] bash c1: started, result unknown"),
        "{}",
        stderr(&output)
    );
    assert!(!dir.path().join("ran-again").exists(), "the command ran again");
}

#[tokio::test]
async fn a_hook_failure_is_reported_with_its_reason() {
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

    let output = run(
        &server,
        dir.path(),
        &["--tools", "bash", "--secret-file", token.to_str().unwrap()],
        "go\n1\n/quit\n",
    )
    .await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let reported = format!(
        "[hook] redact_secrets after_tool failed: cannot read a secret file to redact tool output: {} exceeds 65536 bytes",
        token.display()
    );
    assert!(
        stderr(&output).lines().any(|line| line.starts_with(&reported)),
        "{}",
        stderr(&output)
    );
}
