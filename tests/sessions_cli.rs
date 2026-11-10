//! Session navigation in the compiled program: chat keeps every conversation in a session
//! directory under the home directory, `--resume` continues one, and `zx-agent sessions`
//! lists them. Every test has its own home directory.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use common::mock_server::{MockServer, Reply};
use common::{fake_model_config, stderr, stdout, write_config, zx_agent};
use zx_agent::ai::Message;
use zx_agent::session::{SESSION_VERSION, SessionStore};

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
        common::session_dir(self.home.path(), &self.cwd())
    }

    fn command(&self, args: &[&str]) -> Command {
        write_config(&self.home.path().join(".zx-agent"), &fake_model_config(KEY_VAR, "", ""));
        let mut command = zx_agent();
        command
            .args(args)
            .current_dir(self.work.path())
            .env("HOME", self.home.path())
            .env_remove("ZX_AGENT_HOME")
            .env(KEY_VAR, "sk-sessions-test")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    async fn chat(&self, server: &MockServer, args: &[&str], input: &str) -> Output {
        let mut command = self.command(&["chat", "--model", "mock", "--tools", ""]);
        command.args(args).env("ZX_BASE_URL", &server.base_url);
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

/// The session files in `dir`, by name.
fn files(dir: &Path) -> Vec<PathBuf> {
    common::session_files(dir)
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
    server.requests()[index].user_texts()
}

/// A session file with one prompt, last written at `at_ms`.
fn seed(dir: &Path, id: &str, cwd: &Path, at_ms: u64, prompt: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let header = json!({"type": "session", "version": SESSION_VERSION, "id": id, "timestamp": at_ms - 1,
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
        assert_eq!(mode(&dirs.home.path().join(".zx-agent/sessions")), 0o700);
        assert_eq!(mode(&files[0]), 0o600);
    }
}

#[tokio::test]
async fn the_default_session_directory_follows_zx_agent_home() {
    let dirs = Dirs::new();
    let elsewhere = tempfile::tempdir().unwrap();
    let agent_home = elsewhere.path().join("agent-home");
    let server = MockServer::start(vec![Reply::text("one")]).await;
    let mut command = dirs.command(&["chat", "--model", "mock", "--tools", ""]);
    command
        .env("ZX_BASE_URL", &server.base_url)
        .env("ZX_AGENT_HOME", &agent_home);
    let mut child = command.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"hello\n/quit\n").await.unwrap();
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let encoded = dirs
        .cwd()
        .display()
        .to_string()
        .trim_start_matches('/')
        .replace('/', "-");
    assert_eq!(
        files(&agent_home.join("sessions").join(format!("--{encoded}--"))).len(),
        1
    );
    assert!(!dirs.home.path().join(".zx-agent/sessions").exists());

    let mut listing = dirs.command(&["sessions"]);
    listing.env("ZX_AGENT_HOME", &agent_home);
    let listing = tokio::time::timeout(Duration::from_secs(10), listing.output())
        .await
        .unwrap()
        .unwrap();
    assert!(stdout(&listing).contains("hello"), "{}", stdout(&listing));
}

#[tokio::test]
async fn no_session_keeps_the_conversation_in_memory() {
    let dirs = Dirs::new();
    let server = MockServer::start(vec![Reply::text("one")]).await;

    let output = dirs.chat(&server, &["--no-session"], "hello\n/quit\n").await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!dirs.home.path().join(".zx-agent/sessions").exists());
    assert!(stderr(&output).contains("session=memory"));
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

    let output = dirs.sessions(&[]).await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
    assert!(stderr(&output).contains("[sessions] 0 session(s) in"));
    assert!(!dirs.session_dir().exists(), "listing creates no directory");
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
