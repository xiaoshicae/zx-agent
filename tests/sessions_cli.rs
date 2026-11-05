//! Session navigation in the compiled program (P10 item 6a): chat keeps every conversation
//! in a session directory under the home directory, `--resume` and `/resume` continue one,
//! and `zx-agent sessions` and `/sessions` list them. Every test has its own home directory.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use common::mock_server::{MockServer, Reply};
use zx_agent::ai::Message;
use zx_agent::session::SessionStore;

const KEY_VAR: &str = "ZX_SESSIONS_TEST_KEY";

/// A home directory and a working directory, apart from each other.
struct Dirs {
    home: tempfile::TempDir,
    work: tempfile::TempDir,
}

impl Dirs {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().unwrap(),
            work: tempfile::tempdir().unwrap(),
        }
    }

    /// The working directory as session headers record it.
    fn cwd(&self) -> PathBuf {
        self.work.path().canonicalize().unwrap()
    }

    /// Where chat keeps this working directory's sessions by default, spelled out as pi
    /// spells it.
    fn session_dir(&self) -> PathBuf {
        let cwd = self.cwd().display().to_string();
        let encoded = cwd.trim_start_matches('/').replace('/', "-");
        self.home
            .path()
            .join(".zx-agent/sessions")
            .join(format!("--{encoded}--"))
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_zx-agent"));
        command
            .args(args)
            .current_dir(self.work.path())
            .env("HOME", self.home.path())
            .env(KEY_VAR, "sk-sessions-test")
            .env_remove("ZX_MODEL")
            .env_remove("ZX_BASE_URL")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    async fn chat(&self, server: &MockServer, args: &[&str], input: &str) -> Output {
        let mut command = self.command(&["chat", "--model", "mock", "--base-url", &server.base_url]);
        command
            .args(["--api-key-env", KEY_VAR, "--max-retries", "0", "--tools", ""])
            .args(args);
        let mut child = command.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(input.as_bytes()).await.unwrap();
        drop(stdin);
        tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
            .await
            .unwrap()
            .unwrap()
    }

    async fn sessions(&self, args: &[&str]) -> Output {
        let mut command = self.command(&["sessions"]);
        command.args(args);
        tokio::time::timeout(Duration::from_secs(10), command.output())
            .await
            .unwrap()
            .unwrap()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The session files in `dir`, by name.
fn files(dir: &Path) -> Vec<PathBuf> {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = read_dir
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "jsonl"))
        .collect();
    files.sort();
    files
}

fn prompts(path: &Path) -> Vec<String> {
    SessionStore::read(path)
        .unwrap()
        .messages()
        .into_iter()
        .filter_map(|message| match message {
            Message::User { content, .. } => Some(content),
            _ => None,
        })
        .collect()
}

/// The user messages the model received in request `index`.
fn sent(server: &MockServer, index: usize) -> Vec<String> {
    server.requests()[index].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "user")
        .map(|message| message["content"].as_str().unwrap().to_string())
        .collect()
}

/// A session file with one prompt, last written at `at_ms`.
fn seed(dir: &Path, id: &str, cwd: &Path, at_ms: u64, prompt: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let header = json!({"type": "session", "version": 5, "id": id, "timestamp": at_ms - 1,
        "cwd": cwd.display().to_string(), "model": "mock"});
    let message = json!({"type": "message", "id": format!("{id}-1"), "parentId": null, "timestamp": at_ms,
        "message": {"role": "user", "content": prompt, "timestamp": at_ms}});
    let path = dir.join(format!("{id}.jsonl"));
    std::fs::write(&path, format!("{header}\n{message}\n")).unwrap();
    path
}

#[tokio::test]
async fn chat_keeps_each_conversation_in_its_own_file_under_the_home_directory() {
    let dirs = Dirs::new();
    let server = MockServer::start(vec![Reply::text("one"), Reply::text("two")]).await;

    let first = dirs.chat(&server, &[], "hello\n/quit\n").await;
    let second = dirs.chat(&server, &[], "again\n/quit\n").await;

    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    assert_eq!(second.status.code(), Some(0), "{}", stderr(&second));
    let files = files(&dirs.session_dir());
    assert_eq!(files.len(), 2, "{files:?}");
    let mut prompts: Vec<_> = files.iter().map(|path| prompts(path)).collect();
    prompts.sort();
    assert_eq!(prompts, [["again"], ["hello"]]);
    for path in &files {
        // Named <UTC time>_<session ID>.jsonl.
        let id = SessionStore::read(path).unwrap().header().id.clone();
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(name.ends_with(&format!("_{id}.jsonl")), "{name}");
        assert!(name.starts_with("20") && name.contains('T'), "{name}");
    }
    assert!(stderr(&first).contains(&format!("session={}", dirs.session_dir().display())));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dirs.session_dir()), 0o700);
        assert_eq!(mode(&dirs.home.path().join(".zx-agent")), 0o700);
        assert_eq!(mode(&files[0]), 0o600);
    }
}

#[tokio::test]
async fn no_session_keeps_the_conversation_in_memory() {
    let dirs = Dirs::new();
    let server = MockServer::start(vec![Reply::text("one")]).await;

    let output = dirs.chat(&server, &["--no-session"], "hello\n/sessions\n/quit\n").await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!dirs.home.path().join(".zx-agent").exists());
    assert!(stderr(&output).contains("session=memory"));
    assert!(stderr(&output).contains("[chat] no session directory"));
}

#[tokio::test]
async fn resume_continues_the_most_recent_session_of_this_working_directory() {
    let dirs = Dirs::new();
    let dir = dirs.session_dir();
    seed(&dir, "aaaa0001", &dirs.cwd(), 1_000, "older");
    let newer = seed(&dir, "bbbb0002", &dirs.cwd(), 2_000, "newer");
    // Most recent of all, but started in another directory.
    seed(&dir, "cccc0003", Path::new("/elsewhere"), 3_000, "elsewhere");
    let server = MockServer::start(vec![Reply::text("ok")]).await;

    let output = dirs.chat(&server, &["--resume"], "next\n/quit\n").await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(sent(&server, 0), ["newer", "next"]);
    assert_eq!(prompts(&newer), ["newer", "next"]);
    assert_eq!(files(&dir).len(), 3, "no new file");
    assert!(stderr(&output).contains("resumed 1 message(s) from the session file"));
}

#[tokio::test]
async fn resume_takes_a_session_id_prefix_that_names_one_session() {
    let dirs = Dirs::new();
    let dir = dirs.session_dir();
    let older = seed(&dir, "aaaa0001", &dirs.cwd(), 1_000, "older");
    seed(&dir, "aaab0002", &dirs.cwd(), 2_000, "newer");
    let server = MockServer::start(vec![Reply::text("ok")]).await;

    let output = dirs.chat(&server, &["--resume", "aaaa"], "next\n/quit\n").await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(sent(&server, 0), ["older", "next"]);
    assert_eq!(prompts(&older), ["older", "next"]);

    for (prefix, error) in [
        ("aaa", "more than one session ID starts with aaa"),
        ("zz", "no session ID starts with zz"),
    ] {
        let output = dirs.chat(&server, &["--resume", prefix], "/quit\n").await;
        assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
        assert!(stderr(&output).contains(error), "{}", stderr(&output));
    }
    assert_eq!(files(&dir).len(), 2, "a failed --resume creates no file");
}

#[tokio::test]
async fn resume_without_an_earlier_session_starts_a_new_one() {
    let dirs = Dirs::new();
    let server = MockServer::start(vec![Reply::text("ok")]).await;

    let output = dirs.chat(&server, &["--resume"], "first\n/quit\n").await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stderr(&output).contains("[chat] no earlier session of this directory"));
    let files = files(&dirs.session_dir());
    assert_eq!(files.len(), 1);
    assert_eq!(prompts(&files[0]), ["first"]);
}

#[tokio::test]
async fn session_dir_replaces_the_default_directory() {
    let dirs = Dirs::new();
    let custom = dirs.home.path().join("custom");
    seed(&custom, "aaaa0001", &dirs.cwd(), 1_000, "kept here");
    let server = MockServer::start(vec![Reply::text("ok")]).await;

    let output = dirs
        .chat(
            &server,
            &["--session-dir", custom.to_str().unwrap(), "--resume"],
            "next\n/quit\n",
        )
        .await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(sent(&server, 0), ["kept here", "next"]);
    assert!(!dirs.session_dir().exists());
}

#[tokio::test]
async fn sessions_lists_the_most_recent_first_and_reports_unreadable_files() {
    let dirs = Dirs::new();
    let dir = dirs.session_dir();
    seed(&dir, "aaaa0001-old", &dirs.cwd(), 1_000, "fix the build\nthen test");
    seed(&dir, "bbbb0002-new", &dirs.cwd(), 2_000, "deploy");
    seed(&dir, "cccc0003-far", Path::new("/elsewhere"), 3_000, "elsewhere");
    std::fs::write(dir.join("broken.jsonl"), "not json\n").unwrap();

    let output = dirs.sessions(&[]).await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let lines: Vec<String> = stdout(&output).lines().map(str::to_string).collect();
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert!(lines[0].starts_with("cccc0003  ") && lines[0].ends_with("elsewhere  (cwd /elsewhere)"));
    assert!(lines[1].starts_with("bbbb0002  ") && lines[1].contains("  1 message(s)  deploy"));
    assert!(lines[2].starts_with("aaaa0001  ") && lines[2].ends_with("fix the build…"));
    assert!(stderr(&output).contains("cannot read"));
    assert!(stderr(&output).contains("broken.jsonl"));
    assert!(stderr(&output).contains(&format!("[sessions] 3 session(s) in {}", dir.display())));
}

#[tokio::test]
async fn sessions_finds_none_where_no_session_was_kept() {
    let dirs = Dirs::new();
    let custom = dirs.home.path().join("nothing-here");

    let default = dirs.sessions(&[]).await;
    let explicit = dirs.sessions(&["--session-dir", custom.to_str().unwrap()]).await;

    for output in [default, explicit] {
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        assert_eq!(stdout(&output), "");
        assert!(stderr(&output).contains("[sessions] 0 session(s) in"));
    }
    assert!(!custom.exists(), "listing creates no directory");
}

#[tokio::test]
async fn chat_lists_sessions_and_switches_to_one_by_id() {
    let dirs = Dirs::new();
    let dir = dirs.session_dir();
    let older = seed(&dir, "aaaa0001", &dirs.cwd(), 1_000, "older");
    seed(&dir, "bbbb0002", &dirs.cwd(), 2_000, "newer");
    let server = MockServer::start(vec![Reply::text("ok")]).await;

    let output = dirs
        .chat(
            &server,
            &[],
            "/sessions\n/resume aaaa\nnext\n/resume aaaa\n/resume zz\n/quit\n",
        )
        .await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let err = stderr(&output);
    // The new conversation's own file is listed too, and marked as the current one.
    let current = err
        .lines()
        .find(|line| line.starts_with("* "))
        .unwrap_or_else(|| panic!("{err}"));
    assert!(current.ends_with("(no prompt)"), "{current}");
    assert!(err.contains("  aaaa0001  "), "{err}");
    assert!(err.contains("  bbbb0002  "), "{err}");
    assert!(err.contains("[chat] 3 session(s) in"), "{err}");
    assert!(err.contains(&format!("[chat] session={}", older.display())), "{err}");
    assert!(err.contains("resumed 1 message(s)"), "{err}");
    assert_eq!(sent(&server, 0), ["older", "next"]);
    assert_eq!(prompts(&older), ["older", "next"]);
    assert!(err.contains("[chat] already in session aaaa"), "{err}");
    assert!(err.contains("[chat] no session ID starts with zz"), "{err}");
}

#[tokio::test]
async fn new_starts_another_file_in_the_session_directory() {
    let dirs = Dirs::new();
    let server = MockServer::start(vec![Reply::text("one"), Reply::text("two")]).await;

    let output = dirs.chat(&server, &[], "first\n/new\nsecond\n/quit\n").await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(sent(&server, 1), ["second"]);
    let mut prompts: Vec<_> = files(&dirs.session_dir()).iter().map(|path| prompts(path)).collect();
    prompts.sort();
    assert_eq!(prompts, [["first"], ["second"]]);
}
