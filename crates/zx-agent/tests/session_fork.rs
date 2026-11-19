//! Forking a session (pi: `createBranchedSession`): a new session starts with one
//! branch of another, copied unchanged, and records where it came from.

use std::sync::Arc;

use serde_json::json;
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::{Message, ToolResultMessage};
use zx_agent::events::RunOutcome;
use zx_agent::transcript::{Entry, SESSION_VERSION, SessionHeader, StartRecord, Transcript};
use zx_testing::{RunnerSetup, Seed, config, header, read_lines, tool_call_answer};

fn ids(entries: &[&Entry]) -> Vec<String> {
    entries.iter().map(|entry| entry.id().to_string()).collect()
}

#[tokio::test]
async fn a_fork_copies_one_branch_unchanged_and_names_its_parent() {
    let dir = tempfile::tempdir().unwrap();
    let source_path = dir.path().join("source.jsonl");
    let mut source = zx_testing::create_session_file(&source_path, header()).await.unwrap();
    source.append_message(Message::user("question")).await.unwrap();
    let answer = source.append_message(Message::user("first answer")).await.unwrap();
    source.append_message(Message::user("abandoned")).await.unwrap();
    source.branch(&answer).unwrap();
    let kept = source.append_message(Message::user("kept")).await.unwrap();
    let fork_path = dir.path().join("fork.jsonl");

    let mut fork = zx_testing::create_session_file(&fork_path, source.fork_header(dir.path(), "other-model"))
        .await
        .unwrap();
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
    assert_eq!(header["parentSession"], source_lines[0]["id"]);
    assert_eq!(header["model"], "other-model");
    assert_ne!(header["id"], source_lines[0]["id"]);
    // The fork is an ordinary session: it reopens and takes new entries.
    drop(fork);
    let mut reopened = zx_testing::open_session_file(&fork_path).await.unwrap();
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
    let (runner, _) = RunnerSetup {
        session: zx_testing::create_session_file(&dir.path().join("source.jsonl"), header())
            .await
            .unwrap()
            .into(),
        ..RunnerSetup::new(config(provider.clone(), vec![]))
    }
    .build()
    .await;
    let report = runner.run("go").await;
    assert_eq!(report.outcome, RunOutcome::Completed);
    let source = &runner.transcript();
    let leaf = source.leaf_id().unwrap().to_string();
    let fork_path = dir.path().join("fork.jsonl");

    let mut fork = zx_testing::create_session_file(&fork_path, source.fork_header(dir.path(), "m"))
        .await
        .unwrap();
    fork.copy_branch(source, &leaf).await.unwrap();
    drop(fork);

    let fork = zx_testing::open_session_file(&fork_path).await.unwrap();
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
    let mut source = Transcript::in_memory(header());
    let leaf = source.append_message(Message::user("q")).await.unwrap();

    let mut busy = Transcript::in_memory(source.fork_header(std::path::Path::new("."), "m"));
    busy.append_message(Message::user("already here")).await.unwrap();
    let error = busy.copy_branch(&source, &leaf).await.unwrap_err();
    assert!(error.to_string().contains("empty session"), "{error}");

    let mut fresh = Transcript::in_memory(source.fork_header(std::path::Path::new("."), "m"));
    let error = fresh.copy_branch(&source, "missing").await.unwrap_err();
    assert!(error.to_string().contains("entry missing not found"), "{error}");
    assert!(fresh.entries().is_empty());
    // The parent is named by its ID, so a source that nothing stores has one too.
    assert_eq!(fresh.header().parent_session, Some(source.header().id.clone()));
    assert_eq!(fresh.header().version, SESSION_VERSION);
}

/// The calls a session's branch ends with and has no results for, and what it knows of their
/// starts.
fn unanswered(session: &Transcript) -> Vec<(String, StartRecord)> {
    session
        .interrupted_tool_calls()
        .into_iter()
        .map(|call| (call.tool_call_id, call.start))
        .collect()
}

#[tokio::test]
async fn a_fork_that_ends_at_an_answer_knows_which_of_its_calls_started() {
    let mut source = Seed::in_memory(header()).await;
    source.append_message(Message::user("fix it")).await.unwrap();
    let answer = source
        .append_message(tool_call_answer("p", "m", "bash", json!({})))
        .await
        .unwrap();
    source.record_tool_call_started("c1", "bash").await.unwrap();
    source
        .append_message(Message::ToolResult(ToolResultMessage {
            tool_call_id: "c1".into(),
            tool_name: "bash".into(),
            content: "done".into(),
            is_error: false,
            timestamp: 0,
            duration_ms: None,
            details: None,
            usage: None,
        }))
        .await
        .unwrap();

    let mut fork = Transcript::in_memory(source.fork_header(std::path::Path::new("."), "m"));
    let copied = fork.copy_branch(&source, &answer).await.unwrap();

    // The start and the result follow the answer in the source, so neither is copied; the
    // fork says of the call what the source says once it goes back to the answer.
    assert_eq!(copied, 2);
    source.branch(&answer).unwrap();
    assert_eq!(unanswered(&source), [("c1".to_string(), StartRecord::Present)]);
    assert_eq!(unanswered(&fork), unanswered(&source));
}

#[test]
fn a_fork_keeps_the_labels_of_its_source() {
    let labels = std::collections::BTreeMap::from([("tenant".to_string(), "acme".to_string())]);
    let source = Transcript::in_memory(SessionHeader {
        labels: labels.clone(),
        ..header()
    });

    let fork = source.fork_header(std::path::Path::new("/elsewhere"), "m");

    assert_eq!(fork.labels, labels);
    assert_ne!(fork.id, source.header().id);
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
        parent_session: Some("parent-id".into()),
        ..header()
    };
    assert_eq!(serde_json::to_value(&forked).unwrap()["parentSession"], "parent-id");
}
