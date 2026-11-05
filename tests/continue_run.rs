//! Continuing a conversation without new input (pi: `Agent.continue`): what the session ends
//! with decides what the model answers, and no user message is invented.

mod common;

use std::sync::Arc;

use serde_json::json;
use tokio_util::sync::CancellationToken;

use zx_agent::AgentSession;
use zx_agent::agent::{AgentEvent, Tool};
use zx_agent::agent_session::SessionError;
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::{AssistantMessage, ContentBlock, Message, StopReason};
use zx_agent::runner::{RunOutcome, Runner};
use zx_agent::session::{Entry, SessionStore};

use common::{Collect, EchoTool, RunnerSetup, config, header, summarize};

fn runner(provider: &Arc<FauxProvider>, session: SessionStore) -> (Runner, Arc<Collect>) {
    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(EchoTool::new("echo"))];
    RunnerSetup {
        session,
        ..RunnerSetup::new(config(provider.clone(), tools))
    }
    .build()
}

/// An answer that asked for `echo` and was cut off by a crash before the call ran.
fn call_answer() -> Message {
    let mut answer = AssistantMessage::failure("faux", "test-model", StopReason::ToolUse, "");
    answer.error_message = None;
    answer.content = vec![ContentBlock::ToolCall {
        id: "c1".into(),
        name: "echo".into(),
        arguments: json!({"text": "hi"}),
        arguments_error: None,
    }];
    Message::Assistant(answer)
}

#[tokio::test]
async fn continuing_answers_the_input_the_conversation_ends_with() {
    let mut store = SessionStore::in_memory(header());
    store.append_message(Message::user("question")).await.unwrap();
    let provider = Arc::new(FauxProvider::new([FauxStep::text("answer")]));
    let (mut runner, _) = runner(&provider, store);

    let report = runner.continue_run(&CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    assert_eq!(report.final_text, "answer");
    assert_eq!(summarize(&provider.requests()[0].messages), ["user:question"]);
    assert_eq!(
        summarize(&runner.session().messages()),
        ["user:question", "assistant:answer"]
    );
}

#[tokio::test]
async fn after_a_crash_the_recovered_results_are_answered_without_running_the_call_again() {
    let mut store = SessionStore::in_memory(header());
    store.append_message(Message::user("echo hi")).await.unwrap();
    store.append_message(call_answer()).await.unwrap();
    store.append_tool_call_started("c1", "echo").await.unwrap();
    let provider = Arc::new(FauxProvider::new([FauxStep::text("checked")]));
    let tool = Arc::new(EchoTool::new("echo"));
    let (mut runner, events) = RunnerSetup {
        session: store,
        ..RunnerSetup::new(config(provider.clone(), vec![tool.clone()]))
    }
    .build();

    let report = runner.continue_run(&CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    assert_eq!(tool.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    let sent = summarize(&provider.requests()[0].messages);
    assert_eq!(sent.len(), 3, "{sent:?}");
    assert!(sent[2].starts_with("tool:c1="), "{sent:?}");
    assert!(events.kinds().contains(&"tool_call_recovered"));
}

#[tokio::test]
async fn a_failed_answer_at_the_end_is_omitted_and_answered_again() {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::Fail("HTTP 400: bad request".into()),
        FauxStep::text("second try"),
    ]));
    let (mut runner, events) = runner(&provider, SessionStore::in_memory(header()));
    let failed = runner.run("question", &CancellationToken::new()).await;
    assert_eq!(failed.outcome, RunOutcome::Error);

    let report = runner.continue_run(&CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    assert_eq!(report.final_text, "second try");
    // The prompt is sent once, and the failed answer not at all.
    assert_eq!(summarize(&provider.requests()[1].messages), ["user:question"]);
    assert_eq!(
        summarize(&runner.session().messages()),
        ["user:question", "assistant:second try"]
    );
    // The failed answer stays in the session for the audit.
    let failed_answers = runner
        .session()
        .entries()
        .iter()
        .filter(|entry| matches!(entry, Entry::Message(m) if matches!(&m.message, Message::Assistant(a) if a.stop_reason == StopReason::Error)))
        .count();
    assert_eq!(failed_answers, 1);
    assert!(events.kinds().contains(&"context_edited"));
}

#[tokio::test]
async fn a_finished_answer_leaves_nothing_to_continue_unless_input_is_queued() {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::text("done"),
        FauxStep::text("follow-up answer"),
    ]));
    let (mut runner, _) = runner(&provider, SessionStore::in_memory(header()));
    runner.run("question", &CancellationToken::new()).await;

    let nothing = runner.continue_run(&CancellationToken::new()).await;
    assert_eq!(nothing.outcome, RunOutcome::Error);
    let error = nothing.error.unwrap_or_default();
    assert!(error.contains("ends with a finished answer"), "{error}");
    assert_eq!(nothing.model_requests, 0);
    assert_eq!(provider.requests().len(), 1);

    runner.config().queues.follow_up("and also this");
    let queued = runner.continue_run(&CancellationToken::new()).await;
    assert_eq!(queued.outcome, RunOutcome::Completed, "{queued:?}");
    assert_eq!(
        summarize(&provider.requests()[1].messages),
        ["user:question", "assistant:done", "user:and also this"]
    );
}

#[tokio::test]
async fn an_empty_conversation_has_nothing_to_continue() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("must not be requested")]));
    let (mut runner, events) = runner(&provider, SessionStore::in_memory(header()));

    let report = runner.continue_run(&CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Error);
    assert!(report.error.unwrap_or_default().contains("conversation is empty"));
    assert!(provider.requests().is_empty());
    let kinds = events.kinds();
    assert_eq!(kinds.first(), Some(&"run_start"));
    assert_eq!(kinds.last(), Some(&"run_end"));
}

#[tokio::test]
async fn continuing_requests_are_rebuilt_from_the_session_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let mut store = SessionStore::create(&path, header()).unwrap();
    store.append_message(Message::user("question")).await.unwrap();
    let provider = Arc::new(FauxProvider::new([FauxStep::text("answer")]));
    let (mut runner, _) = runner(&provider, store);

    runner.continue_run(&CancellationToken::new()).await;

    let store = SessionStore::read(&path).unwrap();
    let id = store
        .entries()
        .iter()
        .find_map(|entry| match entry {
            Entry::RequestStarted(started) => Some(started.id.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(store.replay_request(&id).unwrap().1, provider.requests()[0]);
}

#[tokio::test]
async fn a_running_session_refuses_to_continue() {
    let provider = Arc::new(FauxProvider::new([FauxStep::Hang]));
    let (runner, events) = runner(&provider, SessionStore::in_memory(header()));
    let session = AgentSession::from(runner);
    let mut running = Box::pin(session.prompt("first"));
    // Wait until the request is in flight.
    while !events
        .events()
        .iter()
        .any(|event| matches!(event, AgentEvent::RequestStart { .. }))
    {
        tokio::select! {
            _ = tokio::task::yield_now() => {}
            _ = &mut running => panic!("must still be running"),
        }
    }

    assert_eq!(session.continue_run().await.err(), Some(SessionError::Busy));
    session.abort();
    running.await.unwrap();
}
