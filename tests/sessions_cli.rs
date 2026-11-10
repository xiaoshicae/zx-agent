//! Session navigation in the compiled program (P10 items 6a and 6b): chat keeps every
//! conversation in a session directory under the home directory, `--resume` and `/resume`
//! continue one, `--fork` and `/fork` continue in a copy, and `zx-agent sessions` and
//! `/sessions` list them. Every test has its own home directory.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use common::mock_server::{MockServer, Reply};
use common::{stderr, stdout, zx_agent};
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
        let mut command = zx_agent();
        command
            .args(args)
            .current_dir(self.work.path())
            .env("HOME", self.home.path())
            .env_remove("ZX_AGENT_HOME")
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
        assert_eq!(mode(&dirs.home.path().join(".zx-agent")), 0o700);
        assert_eq!(mode(&files[0]), 0o600);
    }
}

#[tokio::test]
async fn the_default_session_directory_follows_zx_agent_home() {
    let dirs = Dirs::new();
    let elsewhere = tempfile::tempdir().unwrap();
    let agent_home = elsewhere.path().join("agent-home");
    let server = MockServer::start(vec![Reply::text("one")]).await;
    let mut command = dirs.command(&["chat", "--model", "mock", "--base-url", &server.base_url]);
    command
        .args(["--api-key-env", KEY_VAR, "--max-retries", "0", "--tools", ""])
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
    assert!(!dirs.home.path().join(".zx-agent").exists());

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

/// The header of a session file.
fn header_of(path: &Path) -> serde_json::Value {
    let text = std::fs::read_to_string(path).unwrap();
    serde_json::from_str(text.lines().next().unwrap()).unwrap()
}

#[tokio::test]
async fn fork_continues_in_a_copy_of_a_session_named_by_id() {
    let dirs = Dirs::new();
    let dir = dirs.session_dir();
    let source = seed(&dir, "aaaa0001", &dirs.cwd(), 1_000, "original");
    let before = std::fs::read(&source).unwrap();
    let server = MockServer::start(vec![Reply::text("ok")]).await;

    let output = dirs.chat(&server, &["--fork", "aaaa"], "next\n/quit\n").await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(sent(&server, 0), ["original", "next"]);
    assert_eq!(std::fs::read(&source).unwrap(), before, "the source is unchanged");
    let files = files(&dir);
    assert_eq!(files.len(), 2);
    let fork = files.iter().find(|path| **path != source).unwrap();
    assert_eq!(prompts(fork), ["original", "next"]);
    let header = header_of(fork);
    assert_eq!(
        header["parentSession"],
        source.canonicalize().unwrap().display().to_string()
    );
    assert_eq!(header["cwd"], dirs.cwd().display().to_string());
    assert!(
        stderr(&output).contains("[chat] forked session aaaa0001 (1 record(s)) into"),
        "{}",
        stderr(&output)
    );
}

#[tokio::test]
async fn fork_takes_a_session_file_from_anywhere() {
    let dirs = Dirs::new();
    let elsewhere = dirs.home.path().join("archive");
    let source = seed(&elsewhere, "bbbb0002", Path::new("/other"), 1_000, "archived");
    let server = MockServer::start(vec![Reply::text("ok")]).await;

    let output = dirs
        .chat(&server, &["--fork", source.to_str().unwrap()], "next\n/quit\n")
        .await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(sent(&server, 0), ["archived", "next"]);
    let files = files(&dirs.session_dir());
    assert_eq!(files.len(), 1, "the copy goes to this directory's sessions");
    assert_eq!(prompts(&files[0]), ["archived", "next"]);
}

#[tokio::test]
async fn fork_of_no_session_fails_without_creating_a_file() {
    let dirs = Dirs::new();
    seed(&dirs.session_dir(), "aaaa0001", &dirs.cwd(), 1_000, "original");
    let server = MockServer::start(vec![]).await;

    let output = dirs.chat(&server, &["--fork", "zz"], "/quit\n").await;

    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("--fork: zz is neither a session file nor a session in"));
    assert_eq!(files(&dirs.session_dir()).len(), 1);
}

#[tokio::test]
async fn slash_fork_continues_in_a_copy_and_keeps_the_original() {
    let dirs = Dirs::new();
    let server = MockServer::start(vec![Reply::text("one"), Reply::text("two")]).await;

    let output = dirs.chat(&server, &[], "first\n/fork\nsecond\n/quit\n").await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(sent(&server, 1), ["first", "second"]);
    let files = files(&dirs.session_dir());
    assert_eq!(files.len(), 2);
    let fork = files
        .iter()
        .find(|path| header_of(path).get("parentSession").is_some())
        .unwrap();
    let original = files.iter().find(|path| *path != fork).unwrap();
    // The conversation goes on in the copy; the original ends where it was forked.
    assert_eq!(prompts(fork), ["first", "second"]);
    assert_eq!(prompts(original), ["first"]);
    // Recorded canonical, like every parent: the home directory may be reached through a link.
    assert_eq!(
        header_of(fork)["parentSession"],
        original.canonicalize().unwrap().display().to_string()
    );
    assert!(stderr(&output).contains("[chat] forked session"), "{}", stderr(&output));
}

#[tokio::test]
async fn slash_fork_needs_a_session_file() {
    let dirs = Dirs::new();
    let server = MockServer::start(vec![]).await;

    let output = dirs.chat(&server, &["--no-session"], "/fork\n/quit\n").await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stderr(&output).contains("[chat] nothing to fork: the conversation is kept in memory"));
    assert!(!dirs.home.path().join(".zx-agent").exists());
}

/// A session file with two exchanges and known IDs: `<id>-1` user `first`, `<id>-2`
/// assistant `one`, `<id>-3` user `second`, `<id>-4` assistant `two`.
fn seed_tree(dir: &Path, id: &str, cwd: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let header = json!({"type": "session", "version": SESSION_VERSION, "id": id, "timestamp": 999,
        "cwd": cwd.display().to_string(), "model": "mock"});
    let mut lines = vec![header.to_string()];
    let mut parent = serde_json::Value::Null;
    for (n, (role, text)) in [
        ("user", "first"),
        ("assistant", "one"),
        ("user", "second"),
        ("assistant", "two"),
    ]
    .into_iter()
    .enumerate()
    {
        let entry_id = format!("{id}-{}", n + 1);
        let at = 1_000 + n as u64;
        let message = if role == "user" {
            json!({"role": "user", "content": text, "timestamp": at})
        } else {
            json!({"role": "assistant", "content": [{"type": "text", "text": text}], "provider": "mock",
                "model": "mock", "usage": {"input": 1, "output": 1, "cacheRead": 0, "totalTokens": 2},
                "stopReason": "stop", "timestamp": at})
        };
        lines.push(
            json!({"type": "message", "id": entry_id, "parentId": parent, "timestamp": at, "message": message})
                .to_string(),
        );
        parent = json!(entry_id);
    }
    let path = dir.join(format!("{id}.jsonl"));
    std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
    path
}

#[tokio::test]
async fn tree_lists_the_branches_and_branch_continues_after_an_entry() {
    let dirs = Dirs::new();
    seed_tree(&dirs.session_dir(), "cccc0003", &dirs.cwd());
    let server = MockServer::start(vec![Reply::text("three")]).await;

    let output = dirs
        .chat(
            &server,
            &["--resume", "cccc"],
            "/tree\n/branch cccc0003-2\nthird\n/tree\n/quit\n",
        )
        .await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let err = stderr(&output);
    // Before the switch: one branch, every entry on it, the position at the end.
    assert!(err.contains("[tree] 4 entries on 1 branch(es);"), "{err}");
    assert!(err.contains("* cccc0003-1"), "{err}");
    assert!(err.contains("> cccc0003-4"), "{err}");
    assert!(err.contains("user       first"), "{err}");
    assert!(err.contains("assistant  two"), "{err}");
    assert!(
        err.contains("[chat] continuing after cccc0003-2 (assistant one); the next message starts a new branch there"),
        "{err}"
    );
    // The model sees the branch, not the exchange left behind.
    assert_eq!(sent(&server, 0), ["first", "third"]);
    // After the switch: the current branch reads straight down, the other is indented.
    assert!(err.contains("    (branch after cccc0003-2)\n      cccc0003-3"), "{err}");
    assert!(err.contains("assistant  three"), "{err}");
    let files = files(&dirs.session_dir());
    assert_eq!(files.len(), 1);
    assert_eq!(prompts(&files[0]), ["first", "third"]);
}

#[tokio::test]
async fn branch_before_a_user_message_lets_the_next_message_take_its_place() {
    let dirs = Dirs::new();
    seed_tree(&dirs.session_dir(), "cccc0003", &dirs.cwd());
    let server = MockServer::start(vec![Reply::text("three")]).await;

    let output = dirs
        .chat(&server, &["--resume", "cccc"], "/branch cccc0003-3\nthird\n/quit\n")
        .await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stderr(&output)
            .contains("[chat] continuing before your message cccc0003-3 (second); the next message takes its place"),
        "{}",
        stderr(&output)
    );
    assert_eq!(sent(&server, 0), ["first", "third"]);
}

#[tokio::test]
async fn fork_copies_the_branch_up_to_the_current_position() {
    let dirs = Dirs::new();
    let original = seed_tree(&dirs.session_dir(), "cccc0003", &dirs.cwd());
    let server = MockServer::start(vec![Reply::text("three")]).await;

    let output = dirs
        .chat(
            &server,
            &["--resume", "cccc"],
            "/branch cccc0003-2\n/fork\nthird\n/quit\n",
        )
        .await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("[chat] forked session cccc0003 (2 record(s)) into"),
        "{}",
        stderr(&output)
    );
    assert_eq!(sent(&server, 0), ["first", "third"]);
    let files = files(&dirs.session_dir());
    assert_eq!(files.len(), 2);
    let fork = files.iter().find(|path| **path != original).unwrap();
    assert_eq!(prompts(fork), ["first", "third"]);
    assert_eq!(prompts(&original), ["first", "second"], "the original is unchanged");
}

#[tokio::test]
async fn branch_needs_an_id_that_names_one_shown_entry() {
    let dirs = Dirs::new();
    seed_tree(&dirs.session_dir(), "cccc0003", &dirs.cwd());
    let server = MockServer::start(vec![]).await;

    let output = dirs
        .chat(
            &server,
            &["--resume", "cccc"],
            "/branch zz\n/branch cccc0003\n/branch cccc0003-4\n/branch\n/quit\n",
        )
        .await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let err = stderr(&output);
    assert!(err.contains("[chat] no entry in /tree starts with zz"), "{err}");
    assert!(
        err.contains("[chat] cccc0003 names more than one entry; /tree shows the full IDs"),
        "{err}"
    );
    assert!(err.contains("[chat] already at that point"), "{err}");
    assert!(
        err.contains("[chat] give an entry ID: /branch <ID>; /tree lists them"),
        "{err}"
    );
    assert!(server.requests().is_empty());
}
