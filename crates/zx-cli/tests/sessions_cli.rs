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
use zx_agent::ai::Message;
use zx_testing::mock_server::{MockServer, Reply};
use zx_testing::program::{Home, ended_output, fake_model_config, seed_session, session_files, write_config};
use zx_testing::{stderr, stdout};

use self::common::zx_agent_in;

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
        zx_testing::program::session_dir(self.home.path(), &self.cwd())
    }

    fn command(&self, args: &[&str]) -> Command {
        write_config(&self.home.path().join(".zx-agent"), &fake_model_config(KEY_VAR, "", ""));
        let mut command = zx_agent_in(self.work.path(), Home::User(self.home.path()));
        command
            .args(args)
            .env(KEY_VAR, "sk-sessions-test")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    async fn chat(&self, server: &MockServer, args: &[&str], input: &str) -> Output {
        let mut command = self.command(&["chat", "--model", "mock", "--tools", ""]);
        command.args(args).env("ZX_BASE_URL", &server.base_url);
        let mut child = command.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(input.as_bytes()).await.unwrap();
        drop(stdin);
        ended_output(child).await
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

fn prompts(path: &Path) -> Vec<String> {
    zx_testing::read_session_file(path)
        .unwrap()
        .messages()
        .into_iter()
        .filter_map(|message| match message {
            Message::User { content, .. } => Some(content),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn chat_keeps_each_conversation_in_its_own_file_under_the_home_directory() {
    let dirs = Dirs::new();
    let server = MockServer::start(vec![Reply::text("one"), Reply::text("two")]).await;

    let first = dirs.chat(&server, &[], "hello\n/quit\n").await;
    let second = dirs.chat(&server, &[], "again\n/quit\n").await;

    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    assert_eq!(second.status.code(), Some(0), "{}", stderr(&second));
    let files = session_files(&dirs.session_dir());
    assert_eq!(files.len(), 2, "{files:?}");
    let mut prompts: Vec<_> = files.iter().map(|path| prompts(path)).collect();
    prompts.sort();
    assert_eq!(prompts, [["again"], ["hello"]]);
    for path in &files {
        // Named <UTC time>_<session ID>.jsonl.
        let id = zx_testing::read_session_file(path).unwrap().header().id.clone();
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
    let output = ended_output(child).await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let encoded = dirs
        .cwd()
        .display()
        .to_string()
        .trim_start_matches('/')
        .replace('/', "-");
    assert_eq!(
        session_files(&agent_home.join("sessions").join(format!("--{encoded}--"))).len(),
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
    seed_session(&dir, "aaaa0001", &dirs.cwd(), 1_000, "older");
    let newer = seed_session(&dir, "bbbb0002", &dirs.cwd(), 2_000, "newer");
    // Most recent of all, but started in another directory.
    seed_session(&dir, "cccc0003", Path::new("/elsewhere"), 3_000, "elsewhere");
    let server = MockServer::start(vec![Reply::text("ok")]).await;

    let output = dirs.chat(&server, &["--resume"], "next\n/quit\n").await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(server.requests()[0].user_texts(), ["newer", "next"]);
    assert_eq!(prompts(&newer), ["newer", "next"]);
    assert_eq!(session_files(&dir).len(), 3, "no new file");
    assert!(stderr(&output).contains("resumed 1 message(s) from the session file"));
}

#[tokio::test]
async fn resume_takes_a_session_id_prefix_that_names_one_session() {
    let dirs = Dirs::new();
    let dir = dirs.session_dir();
    let older = seed_session(&dir, "aaaa0001", &dirs.cwd(), 1_000, "older");
    seed_session(&dir, "aaab0002", &dirs.cwd(), 2_000, "newer");
    let server = MockServer::start(vec![Reply::text("ok")]).await;

    let output = dirs.chat(&server, &["--resume", "aaaa"], "next\n/quit\n").await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(server.requests()[0].user_texts(), ["older", "next"]);
    assert_eq!(prompts(&older), ["older", "next"]);

    for (prefix, error) in [
        ("aaa", "more than one session ID starts with aaa"),
        ("zz", "no session ID starts with zz"),
    ] {
        let output = dirs.chat(&server, &["--resume", prefix], "/quit\n").await;
        assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
        assert!(stderr(&output).contains(error), "{}", stderr(&output));
    }
    assert_eq!(session_files(&dir).len(), 2, "a failed --resume creates no file");
}

#[tokio::test]
async fn resume_without_an_earlier_session_starts_a_new_one() {
    let dirs = Dirs::new();
    let server = MockServer::start(vec![Reply::text("ok")]).await;

    let output = dirs.chat(&server, &["--resume"], "first\n/quit\n").await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stderr(&output).contains("[chat] no earlier session of this directory"));
    let files = session_files(&dirs.session_dir());
    assert_eq!(files.len(), 1);
    assert_eq!(prompts(&files[0]), ["first"]);
}

#[tokio::test]
async fn sessions_lists_the_most_recent_first_and_reports_unreadable_files() {
    let dirs = Dirs::new();
    let dir = dirs.session_dir();
    seed_session(&dir, "aaaa0001-old", &dirs.cwd(), 1_000, "fix the build\nthen test");
    seed_session(&dir, "bbbb0002-new", &dirs.cwd(), 2_000, "deploy");
    // Started in another directory, whose name leads to the same session directory.
    seed_session(&dir, "cccc0003-far", Path::new("/elsewhere"), 3_000, "elsewhere");
    std::fs::write(dir.join("broken.jsonl"), "not json\n").unwrap();

    let output = dirs.sessions(&[]).await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let lines: Vec<String> = stdout(&output).lines().map(str::to_string).collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].starts_with("bbbb0002  ") && lines[0].contains("  1 message(s)  deploy"));
    assert!(lines[1].starts_with("aaaa0001  ") && lines[1].ends_with("fix the build…"));
    let errors = stderr(&output);
    assert!(errors.contains("broken.jsonl: invalid session header"), "{errors}");
    assert!(errors.contains(&format!("[sessions] 2 session(s) in {}", dir.display())));
}

#[tokio::test]
async fn sessions_names_an_unreadable_file_once() {
    let dirs = Dirs::new();
    let dir = dirs.session_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("broken.jsonl"), "not json\n").unwrap();

    let output = dirs.sessions(&[]).await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let errors = stderr(&output);
    let line = errors
        .lines()
        .find(|line| line.contains("broken.jsonl"))
        .unwrap_or_else(|| panic!("{errors}"));
    assert_eq!(line.matches("broken.jsonl").count(), 1, "{line}");
}

#[tokio::test]
async fn sessions_lists_a_named_session_by_its_latest_name() {
    let dirs = Dirs::new();
    let path = seed_session(&dirs.session_dir(), "aaaa0001", &dirs.cwd(), 1_000, "fix the build");
    let names: Vec<String> = ["first name", "build fix"]
        .iter()
        .enumerate()
        .map(|(n, name)| {
            json!({"type": "session_info", "id": format!("aaaa0001-{}", n + 2),
                "parentId": format!("aaaa0001-{}", n + 1), "timestamp": 1_001 + n, "name": name})
            .to_string()
        })
        .collect();
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str(&format!("{}\n", names.join("\n")));
    std::fs::write(&path, text).unwrap();

    let output = dirs.sessions(&[]).await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let listed = stdout(&output);
    assert!(
        listed.starts_with("aaaa0001  ") && listed.ends_with("  1 message(s)  build fix\n"),
        "{listed}"
    );
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
    assert_eq!(server.requests()[1].user_texts(), ["second"]);
    let mut prompts: Vec<_> = session_files(&dirs.session_dir())
        .iter()
        .map(|path| prompts(path))
        .collect();
    prompts.sort();
    assert_eq!(prompts, [["first"], ["second"]]);
}
