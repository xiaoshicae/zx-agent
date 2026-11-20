//! Continuing a conversation without new input (pi: `Agent.continue`): what the session ends
//! with decides what the model answers, and no user message is invented.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::{AssistantMessage, Message, ProviderErrorKind, StopReason};
use zx_agent::events::RunOutcome;
use zx_agent::hooks::{Hook, HookContext, HookResult};
use zx_agent::input::{Input, QueueMode};
use zx_agent::session::SessionError;
use zx_agent::tools::Tool;
use zx_agent::transcript::{CustomMessage, Entry};
use zx_testing::{
    Collect, Driven, EchoTool, NotifyingProvider, RunnerSetup, Seed, Start, config, header, replayed_requests,
    summarize, take_queued_inputs, tool_call_answer, tool_call_block,
};

async fn runner(provider: &Arc<FauxProvider>, session: Start) -> (Driven, Arc<Collect>) {
    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(EchoTool::new("echo"))];
    RunnerSetup {
        session,
        ..RunnerSetup::new(config(provider.clone(), tools))
    }
    .build()
    .await
}

/// An answer that asked for `echo` and was cut off by a crash before the call ran.
fn call_answer() -> Message {
    tool_call_answer("faux", "test-model", "echo", json!({"text": "hi"}))
}

#[tokio::test]
async fn continuing_answers_the_input_the_conversation_ends_with() {
    let mut seed = Seed::in_memory(header()).await;
    seed.append_message(Message::user("question")).await.unwrap();
    let provider = Arc::new(FauxProvider::new([FauxStep::text("answer")]));
    let (runner, _) = runner(&provider, seed.into()).await;

    let report = runner.continue_run().await.unwrap();

    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    assert_eq!(report.final_text, "answer");
    assert_eq!(summarize(&provider.requests()[0].messages), ["user:question"]);
    assert_eq!(
        summarize(&runner.transcript().messages()),
        ["user:question", "assistant:answer"]
    );
}

#[tokio::test]
async fn after_a_crash_the_recovered_results_are_answered_without_running_the_call_again() {
    let mut seed = Seed::in_memory(header()).await;
    seed.append_message(Message::user("echo hi")).await.unwrap();
    seed.append_message(call_answer()).await.unwrap();
    seed.record_tool_call_started("c1", "echo").await.unwrap();
    let provider = Arc::new(FauxProvider::new([FauxStep::text("checked")]));
    let tool = Arc::new(EchoTool::new("echo"));
    let (runner, events) = RunnerSetup {
        session: seed.into(),
        ..RunnerSetup::new(config(provider.clone(), vec![tool.clone()]))
    }
    .build()
    .await;

    let report = runner.continue_run().await.unwrap();

    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    assert_eq!(tool.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    let sent = summarize(&provider.requests()[0].messages);
    assert_eq!(sent.len(), 3, "{sent:?}");
    assert!(sent[2].starts_with("tool:c1="), "{sent:?}");
    assert!(events.kinds().contains(&"tool_call_recovered"));
}

#[tokio::test]
async fn a_message_recorded_while_idle_after_a_crash_follows_the_recovered_results() {
    let mut seed = Seed::in_memory(header()).await;
    seed.append_message(Message::user("echo hi")).await.unwrap();
    seed.append_message(call_answer()).await.unwrap();
    seed.record_tool_call_started("c1", "echo").await.unwrap();
    let provider = Arc::new(FauxProvider::new([FauxStep::text("checked")]));
    let (runner, _) = runner(&provider, seed.into()).await;
    let heard = CustomMessage {
        custom_type: "heard".into(),
        content: "someone spoke".into(),
        display: true,
        details: None,
    };
    runner.append_custom_message(heard).await.unwrap();

    let report = runner.run("what happened?").await;

    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    let sent = summarize(&provider.requests()[0].messages);
    assert!(sent[2].starts_with("tool:c1=Result unknown"), "{sent:?}");
}

#[tokio::test]
async fn a_failed_answer_with_calls_at_the_end_is_omitted_and_answered_again() {
    let mut seed = Seed::in_memory(header()).await;
    seed.append_message(Message::user("echo hi")).await.unwrap();
    let mut failed = AssistantMessage::failure("faux", "test-model", StopReason::Error, "the stream broke");
    failed.content = vec![tool_call_block("c1", "echo", json!({"text": "h"}))];
    seed.append_message(Message::Assistant(failed)).await.unwrap();
    let provider = Arc::new(FauxProvider::new([FauxStep::text("second try")]));
    let (runner, _) = runner(&provider, seed.into()).await;

    let report = runner.continue_run().await.unwrap();

    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    assert_eq!(summarize(&provider.requests()[0].messages), ["user:echo hi"]);
}

#[tokio::test]
async fn a_failed_answer_at_the_end_is_omitted_and_answered_again() {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::fail(ProviderErrorKind::Other, "HTTP 400: bad request"),
        FauxStep::text("second try"),
    ]));
    let (runner, events) = runner(&provider, Start::default()).await;
    let failed = runner.run("question").await;
    assert_eq!(failed.outcome, RunOutcome::Failed);

    let report = runner.continue_run().await.unwrap();

    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    assert_eq!(report.final_text, "second try");
    // The prompt is sent once, and the failed answer not at all.
    assert_eq!(summarize(&provider.requests()[1].messages), ["user:question"]);
    assert_eq!(
        summarize(&runner.transcript().messages()),
        ["user:question", "assistant:second try"]
    );
    // The failed answer stays in the session for the audit.
    let failed_answers = runner
        .transcript()
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
    let (runner, _) = runner(&provider, Start::default()).await;
    runner.run("question").await;

    let nothing = runner.continue_run().await.unwrap();
    assert_eq!(nothing.outcome, RunOutcome::Failed);
    let error = nothing.error.unwrap_or_default();
    assert!(error.contains("ends with a finished answer"), "{error}");
    assert_eq!(nothing.model_requests, 0);
    assert_eq!(provider.requests().len(), 1);

    runner.queues().follow_up("and also this");
    let queued = runner.continue_run().await.unwrap();
    assert_eq!(queued.outcome, RunOutcome::Completed, "{queued:?}");
    assert_eq!(
        summarize(&provider.requests()[1].messages),
        ["user:question", "assistant:done", "user:and also this"]
    );
}

/// Refuses the input "refused" and lets every other pass.
struct RefuseMarked;

#[async_trait]
impl Hook for RefuseMarked {
    async fn on_input(&self, text: String, _ctx: &HookContext) -> HookResult<String> {
        if text == "refused" {
            Err("not this one".into())
        } else {
            Ok(text)
        }
    }
}

#[tokio::test]
async fn a_continuation_that_stops_at_a_queued_message_leaves_the_rest_in_the_queue() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("done")]));
    let mut config = config(provider.clone(), vec![]);
    config.hooks.push("refuse_marked", Arc::new(RefuseMarked));
    let (runner, _) = RunnerSetup::new(config).build().await;
    runner.run("question").await;
    runner.queues().follow_up("refused");
    runner.queues().follow_up("kept");

    let report = runner.continue_run().await.unwrap();

    assert_eq!(report.outcome, RunOutcome::Failed, "{report:?}");
    assert_eq!(provider.requests().len(), 1, "the refused input is not sent");
    // The run took the first message and stopped there: the second is still the host's.
    assert_eq!(take_queued_inputs(&runner).unwrap(), [Input::from("kept")]);
}

#[tokio::test]
async fn a_continuation_takes_queued_follow_ups_one_at_a_time_when_the_queue_says_so() {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::text("done"),
        FauxStep::text("first answered"),
        FauxStep::text("second answered"),
    ]));
    let (runner, _) = runner(&provider, Start::default()).await;
    runner.run("question").await;
    runner.queues().set_mode(QueueMode::OneAtATime);
    runner.queues().follow_up("first");
    runner.queues().follow_up("second");

    let report = runner.continue_run().await.unwrap();

    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    // Each follow-up gets a request of its own, as it does between the steps of a run.
    assert_eq!(
        summarize(&provider.requests()[1].messages),
        ["user:question", "assistant:done", "user:first"]
    );
    assert_eq!(provider.requests().len(), 3);
}

#[tokio::test]
async fn an_empty_conversation_has_nothing_to_continue() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("must not be requested")]));
    let (runner, events) = runner(&provider, Start::default()).await;

    let report = runner.continue_run().await.unwrap();

    assert_eq!(report.outcome, RunOutcome::Failed);
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
    let mut seed = zx_testing::create_session_file(&path, header()).await.unwrap();
    seed.append_message(Message::user("question")).await.unwrap();
    let provider = Arc::new(FauxProvider::new([FauxStep::text("answer")]));
    let (runner, _) = runner(&provider, seed.into()).await;

    runner.continue_run().await.unwrap();

    let transcript = zx_testing::read_session_file(&path).unwrap();
    assert_eq!(replayed_requests(&transcript), provider.requests());
}

#[tokio::test]
async fn a_running_session_refuses_to_continue() {
    let provider = NotifyingProvider::new(FauxProvider::new([FauxStep::Hang]));
    let (session, _) = RunnerSetup::new(config(provider.clone(), vec![])).build().await;
    let mut running = Box::pin(session.prompt("first"));
    provider.wait_for_start(&mut running).await;

    assert_eq!(session.continue_run().await.err(), Some(SessionError::Busy));
    session.cancel();
    running.await.unwrap();
}
