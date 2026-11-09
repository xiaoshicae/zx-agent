//! Switching branches within a session (P10 item 6c): `AgentSession::branch` moves the
//! conversation to another point of its tree, `inspect` reads the tree while idle.
mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use serde_json::json;
use tokio_util::sync::CancellationToken;

use common::{EchoTool, NotifyingProvider, RunnerSetup, config, header, summarize};
use zx_agent::AgentSession;
use zx_agent::agent::Tool;
use zx_agent::agent_session::AgentSessionError;
use zx_agent::ai::Message;
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::runner::RunOutcome;
use zx_agent::session::{Entry, SessionStore};

fn file_session(file: &Path, provider: Arc<FauxProvider>, tools: Vec<Arc<dyn Tool>>) -> AgentSession {
    let (runner, _) = RunnerSetup {
        session: SessionStore::create(file, header()).unwrap(),
        ..RunnerSetup::new(config(provider, tools))
    }
    .build();
    AgentSession::from(runner)
}

/// The ID of the first message entry `matches` accepts.
fn entry_id(session: &AgentSession, matches: impl Fn(&Message) -> bool) -> String {
    session
        .inspect(|store| {
            store.entries().iter().find_map(|entry| match entry {
                Entry::Message(entry) if matches(&entry.message) => Some(entry.id.clone()),
                _ => None,
            })
        })
        .unwrap()
        .expect("a matching entry")
}

fn answer_id(session: &AgentSession, text: &str) -> String {
    entry_id(
        session,
        |message| matches!(message, Message::Assistant(answer) if answer.text() == text),
    )
}

#[tokio::test]
async fn branching_to_an_earlier_answer_continues_there_and_keeps_the_branch_left_behind() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("session.jsonl");
    let provider = Arc::new(FauxProvider::new([
        FauxStep::text("one"),
        FauxStep::text("two"),
        FauxStep::text("three"),
    ]));
    let session = file_session(&file, provider.clone(), vec![]);
    session.prompt("first", &CancellationToken::new()).await.unwrap();
    session.prompt("second", &CancellationToken::new()).await.unwrap();
    let one = answer_id(&session, "one");

    session.branch(Some(&one)).unwrap();
    let report = session.prompt("third", &CancellationToken::new()).await.unwrap();

    assert_eq!(report.final_text, "three");
    assert_eq!(
        summarize(&provider.requests()[2].messages),
        ["user:first", "assistant:one", "user:third"]
    );
    // The new input hangs off the answer it was branched to.
    let third = entry_id(
        &session,
        |message| matches!(message, Message::User { content, .. } if content == "third"),
    );
    let parent = session
        .inspect(|store| {
            store
                .get(&third)
                .and_then(|entry| entry.parent_id().map(str::to_string))
        })
        .unwrap();
    assert_eq!(parent.as_deref(), Some(one.as_str()));
    drop(session);
    // Reopened, the file continues where it ends: on the new branch; the old one is kept.
    let reopened = SessionStore::open(&file).unwrap();
    assert_eq!(
        summarize(&reopened.messages()),
        ["user:first", "assistant:one", "user:third", "assistant:three"]
    );
    let users = reopened
        .entries()
        .iter()
        .filter(|entry| matches!(entry, Entry::Message(entry) if matches!(entry.message, Message::User { .. })))
        .count();
    assert_eq!(users, 3, "the branch left behind stays in the file");
}

#[tokio::test]
async fn a_prompt_after_an_answer_with_unanswered_calls_records_them_as_not_executed() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("session.jsonl");
    let echo = Arc::new(EchoTool::new("echo"));
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c1", "echo", json!({"text": "hi"})),
        FauxStep::text("done"),
        FauxStep::text("again"),
    ]));
    let session = file_session(&file, provider.clone(), vec![echo.clone()]);
    session.prompt("first", &CancellationToken::new()).await.unwrap();
    assert_eq!(echo.calls.load(Ordering::SeqCst), 1);
    let calling = entry_id(
        &session,
        |message| matches!(message, Message::Assistant(answer) if !answer.tool_calls().is_empty()),
    );

    session.branch(Some(&calling)).unwrap();
    let report = session.prompt("second", &CancellationToken::new()).await.unwrap();

    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(echo.calls.load(Ordering::SeqCst), 1, "the call is not run again");
    let context = summarize(&provider.requests()[2].messages);
    assert_eq!(&context[..2], ["user:first", "assistant:calls[c1]"]);
    assert!(
        context[2].starts_with("tool:c1=Not executed:") && context[2].ends_with('!'),
        "{}",
        context[2]
    );
    assert_eq!(context[3], "user:second");
}

#[tokio::test]
async fn branching_to_the_root_starts_the_conversation_over_in_the_same_session() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("session.jsonl");
    let provider = Arc::new(FauxProvider::new([FauxStep::text("one"), FauxStep::text("fresh")]));
    let session = file_session(&file, provider.clone(), vec![]);
    session.prompt("first", &CancellationToken::new()).await.unwrap();

    session.branch(None).unwrap();
    session.prompt("new", &CancellationToken::new()).await.unwrap();

    assert_eq!(summarize(&provider.requests()[1].messages), ["user:new"]);
    drop(session);
    let reopened = SessionStore::open(&file).unwrap();
    assert_eq!(summarize(&reopened.messages()), ["user:new", "assistant:fresh"]);
    let roots = reopened
        .entries()
        .iter()
        .filter(|entry| entry.parent_id().is_none())
        .count();
    assert_eq!(roots, 2);
}

#[tokio::test]
async fn branching_and_inspecting_wait_for_the_run_and_reject_unknown_entries() {
    let provider = NotifyingProvider::new([FauxStep::Hang, FauxStep::text("later")]);
    let (runner, _) = RunnerSetup::new(config(provider.clone(), vec![])).build();
    let session = AgentSession::from(runner);
    let cancel = CancellationToken::new();
    let mut running = Box::pin(session.prompt("first", &cancel));
    tokio::select! {
        () = provider.started.notified() => {}
        _ = &mut running => panic!("must be running"),
    }
    assert_eq!(session.branch(None), Err(AgentSessionError::Busy));
    assert_eq!(
        session.inspect(|store| store.entries().len()).err(),
        Some(AgentSessionError::Busy)
    );
    assert!(session.abort());
    assert_eq!(running.await.unwrap().outcome, RunOutcome::Aborted);

    assert_eq!(session.branch(Some("nowhere")), Err(AgentSessionError::EntryNotFound));
    // The position did not move: the next prompt continues after the aborted answer.
    session.prompt("second", &CancellationToken::new()).await.unwrap();
    let context = summarize(&provider.requests()[1].messages);
    assert_eq!(context[0], "user:first");
    assert_eq!(context[context.len() - 1], "user:second");
    // While `inspect` runs, the conversation is leased, as during a prompt.
    let during = session.inspect(|_| session.set_reasoning(None)).unwrap();
    assert_eq!(during, Err(AgentSessionError::Busy));
    assert_eq!(session.set_reasoning(None), Ok(()));
}
