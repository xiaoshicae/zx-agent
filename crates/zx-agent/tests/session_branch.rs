//! Switching branches within a session: `Session::branch` moves the
//! conversation to another point of its tree, `inspect` reads the tree while idle.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use serde_json::json;

use zx_agent::ai::Message;
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::events::RunOutcome;
use zx_agent::session::SessionError;
use zx_agent::tools::Tool;
use zx_agent::transcript::Entry;
use zx_testing::{Driven, EchoTool, NotifyingProvider, RunnerSetup, config, header, summarize};

async fn file_session(file: &Path, provider: Arc<FauxProvider>, tools: Vec<Arc<dyn Tool>>) -> Driven {
    let (runner, _) = RunnerSetup {
        session: zx_testing::create_session_file(file, header()).await.unwrap().into(),
        ..RunnerSetup::new(config(provider, tools))
    }
    .build()
    .await;
    runner
}

/// The ID of the first message entry `matches` accepts.
fn entry_id(session: &Driven, matches: impl Fn(&Message) -> bool) -> String {
    session
        .with_transcript(|store| {
            store.entries().iter().find_map(|entry| match entry {
                Entry::Message(entry) if matches(&entry.message) => Some(entry.id.clone()),
                _ => None,
            })
        })
        .unwrap()
        .expect("a matching entry")
}

fn answer_id(session: &Driven, text: &str) -> String {
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
    let session = file_session(&file, provider.clone(), vec![]).await;
    session.prompt("first").await.unwrap();
    session.prompt("second").await.unwrap();
    let one = answer_id(&session, "one");

    session.branch(Some(&one)).unwrap();
    let report = session.prompt("third").await.unwrap();

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
        .with_transcript(|store| {
            store
                .get(&third)
                .and_then(|entry| entry.parent_id().map(str::to_string))
        })
        .unwrap();
    assert_eq!(parent.as_deref(), Some(one.as_str()));
    drop(session);
    // Reopened, the file continues where it ends: on the new branch; the old one is kept.
    let reopened = zx_testing::open_session_file(&file).await.unwrap();
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
async fn a_prompt_after_going_back_to_an_answer_tells_the_model_that_its_calls_started() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("session.jsonl");
    let echo = Arc::new(EchoTool::new("echo"));
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c1", "echo", json!({"text": "hi"})),
        FauxStep::text("done"),
        FauxStep::text("again"),
    ]));
    let session = file_session(&file, provider.clone(), vec![echo.clone()]).await;
    session.prompt("first").await.unwrap();
    assert_eq!(echo.calls.load(Ordering::SeqCst), 1);
    let calling = entry_id(
        &session,
        |message| matches!(message, Message::Assistant(answer) if !answer.tool_calls().is_empty()),
    );

    session.branch(Some(&calling)).unwrap();
    let report = session.prompt("second").await.unwrap();

    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(echo.calls.load(Ordering::SeqCst), 1, "the call is not run again");
    let context = summarize(&provider.requests()[2].messages);
    assert_eq!(&context[..2], ["user:first", "assistant:calls[c1]"]);
    // The call ran on the branch that was left: going back did not undo it.
    assert!(
        context[2].starts_with("tool:c1=Result unknown: this call started") && context[2].ends_with('!'),
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
    let session = file_session(&file, provider.clone(), vec![]).await;
    session.prompt("first").await.unwrap();

    session.branch(None).unwrap();
    session.prompt("new").await.unwrap();

    assert_eq!(summarize(&provider.requests()[1].messages), ["user:new"]);
    drop(session);
    let reopened = zx_testing::open_session_file(&file).await.unwrap();
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
    let provider = NotifyingProvider::new(FauxProvider::new([FauxStep::Hang, FauxStep::text("later")]));
    let (session, _) = RunnerSetup::new(config(provider.clone(), vec![])).build().await;
    let mut running = Box::pin(session.prompt("first"));
    tokio::select! {
        () = provider.started.notified() => {}
        _ = &mut running => panic!("must be running"),
    }
    assert_eq!(session.branch(None), Err(SessionError::Busy));
    assert_eq!(
        session.with_transcript(|store| store.entries().len()).err(),
        Some(SessionError::Busy)
    );
    assert!(session.cancel());
    assert_eq!(running.await.unwrap().outcome, RunOutcome::Cancelled);

    assert_eq!(session.branch(Some("nowhere")), Err(SessionError::EntryNotFound));
    // The position did not move: the next prompt continues after the aborted answer.
    session.prompt("second").await.unwrap();
    let context = summarize(&provider.requests()[1].messages);
    assert_eq!(context[0], "user:first");
    assert_eq!(context[context.len() - 1], "user:second");
    // While `inspect` runs, the conversation is leased, as during a prompt.
    let during = session.with_transcript(|_| session.set_reasoning(None)).unwrap();
    assert_eq!(during, Err(SessionError::Busy));
    assert_eq!(session.set_reasoning(None), Ok(()));
}
