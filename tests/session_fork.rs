//! Forking a session (P10 item 6b; pi: `createBranchedSession`): a new session starts with one
//! branch of another, copied unchanged, and records where it came from.

mod common;

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use common::{RunnerSetup, config, header, read_lines};
use zx_agent::ai::Message;
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::runner::RunOutcome;
use zx_agent::session::{Entry, SESSION_VERSION, SessionHeader, SessionStore};

fn ids(entries: &[&Entry]) -> Vec<String> {
    entries.iter().map(|entry| entry.id().to_string()).collect()
}

#[tokio::test]
async fn a_fork_copies_one_branch_unchanged_and_names_its_parent() {
    let dir = tempfile::tempdir().unwrap();
    let source_path = dir.path().join("source.jsonl");
    let mut source = SessionStore::create(&source_path, header()).unwrap();
    source.append_message(Message::user("question")).await.unwrap();
    let answer = source.append_message(Message::user("first answer")).await.unwrap();
    source.append_message(Message::user("abandoned")).await.unwrap();
    source.branch(&answer).unwrap();
    let kept = source.append_message(Message::user("kept")).await.unwrap();
    let fork_path = dir.path().join("fork.jsonl");

    let mut fork = SessionStore::create(&fork_path, source.fork_header(dir.path(), "other-model")).unwrap();
    let copied = fork.copy_branch(&source, &kept).await.unwrap();

    assert_eq!(copied, 3);
    assert_eq!(ids(&fork.branch_path()), ids(&source.branch_path()));
    assert_eq!(fork.leaf_id(), Some(kept.as_str()));
    assert_eq!(fork.messages(), source.messages());
    // Entries are written byte for byte as the source wrote them.
    let source_lines = read_lines(&source_path);
    let fork_lines = read_lines(&fork_path);
    let abandoned = &source_lines[3];
    assert_eq!(
        fork_lines[1..],
        [
            source_lines[1].clone(),
            source_lines[2].clone(),
            source_lines[4].clone()
        ]
    );
    assert!(!fork_lines.contains(abandoned));
    let header = &fork_lines[0];
    assert_eq!(header["parentSession"], source_path.display().to_string());
    assert_eq!(header["model"], "other-model");
    assert_ne!(header["id"], source_lines[0]["id"]);
    // The fork is an ordinary session: it reopens and takes new entries.
    drop(fork);
    let mut reopened = SessionStore::open(&fork_path).unwrap();
    reopened.append_message(Message::user("more")).await.unwrap();
    assert_eq!(reopened.messages().len(), 4);
    assert_eq!(
        read_lines(&source_path).len(),
        source_lines.len(),
        "the source is unchanged"
    );
}

#[tokio::test]
async fn requests_of_the_copied_branch_replay_from_the_fork() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(FauxProvider::new([FauxStep::text("answer")]));
    let (mut runner, _) = RunnerSetup {
        session: SessionStore::create(&dir.path().join("source.jsonl"), header()).unwrap(),
        ..RunnerSetup::new(config(provider.clone(), vec![]))
    }
    .build();
    let report = runner.run("go", &CancellationToken::new()).await;
    assert_eq!(report.outcome, RunOutcome::Completed);
    let source = runner.session();
    let leaf = source.leaf_id().unwrap().to_string();
    let fork_path = dir.path().join("fork.jsonl");

    let mut fork = SessionStore::create(&fork_path, source.fork_header(dir.path(), "m")).unwrap();
    fork.copy_branch(source, &leaf).await.unwrap();
    drop(fork);

    let fork = SessionStore::open(&fork_path).unwrap();
    let replayed: Vec<_> = fork
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            Entry::RequestStarted(started) => Some(fork.replay_request(&started.id).unwrap().1),
            _ => None,
        })
        .collect();
    assert_eq!(replayed, provider.requests());
}

#[tokio::test]
async fn a_branch_is_copied_only_into_an_empty_session() {
    let mut source = SessionStore::in_memory(header());
    let leaf = source.append_message(Message::user("q")).await.unwrap();

    let mut busy = SessionStore::in_memory(source.fork_header(std::path::Path::new("."), "m"));
    busy.append_message(Message::user("already here")).await.unwrap();
    let error = busy.copy_branch(&source, &leaf).await.unwrap_err();
    assert!(error.to_string().contains("empty session"), "{error}");

    let mut fresh = SessionStore::in_memory(source.fork_header(std::path::Path::new("."), "m"));
    let error = fresh.copy_branch(&source, "missing").await.unwrap_err();
    assert!(error.to_string().contains("entry missing not found"), "{error}");
    assert!(fresh.entries().is_empty());
    // An in-memory source has no file to name.
    assert_eq!(fresh.header().parent_session, None);
    assert_eq!(fresh.header().version, SESSION_VERSION);
}

#[test]
fn headers_without_a_parent_keep_their_format() {
    let plain = header();
    let json = serde_json::to_value(&plain).unwrap();
    assert!(json.get("parentSession").is_none(), "{json}");
    // A header without parentSession reads back without a parent.
    let old: SessionHeader = serde_json::from_value(json).unwrap();
    assert_eq!(old.parent_session, None);
    let forked = SessionHeader {
        parent_session: Some("/s/a.jsonl".into()),
        ..header()
    };
    assert_eq!(serde_json::to_value(&forked).unwrap()["parentSession"], "/s/a.jsonl");
}
