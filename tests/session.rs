//! Shared conversation ownership, cancellation, the run a prompt returns, and model
//! switching.
mod common;

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use common::{Collect, Driven, NotifyingProvider, RunnerSetup, config, header, summarize};
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::{Reasoning, Request, ToolDef};
use zx_agent::events::EventKind;
use zx_agent::input::Input;
use zx_agent::policy::{ApprovalDecision, ApprovalMode, PolicyContext, PolicyDecision, ToolPolicy};
use zx_agent::runner::RunOutcome;
use zx_agent::session::{ModelRef, SessionError};
use zx_agent::tools::{Tool, ToolContext, ToolOutput};
use zx_agent::transcript::{Entry, SessionName, Transcript};
use zx_agent::{Run, Session};

fn setup(steps: impl IntoIterator<Item = FauxStep>) -> (Driven, Arc<NotifyingProvider>, Arc<Collect>) {
    let provider = NotifyingProvider::new(steps);
    let (runner, events) = RunnerSetup::new(config(provider.clone(), vec![])).build();
    (runner.into_session(), provider, events)
}

#[tokio::test]
async fn sequential_prompts_keep_history_and_each_reports_its_own_answer() {
    let (session, provider, _) = setup([FauxStep::text("one"), FauxStep::text("two")]);
    let first = session.prompt("first", &CancellationToken::new()).await.unwrap();
    let second = session.prompt("second", &CancellationToken::new()).await.unwrap();
    assert_eq!((first.final_text.as_str(), second.final_text.as_str()), ("one", "two"));
    assert_eq!(
        summarize(&provider.faux.requests()[1].messages),
        ["user:first", "assistant:one", "user:second"]
    );
    assert_eq!(second.usage.total_tokens, 15);
}

#[tokio::test]
async fn sdk_prompts_preserve_empty_and_unicode_input() {
    let (session, provider, _) = setup([FauxStep::text("空"), FauxStep::text("你好")]);
    session.prompt("", &CancellationToken::new()).await.unwrap();
    let report = session
        .prompt("多字节输入 🦀", &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.final_text, "你好");
    let requests = provider.faux.requests();
    assert_eq!(summarize(&requests[0].messages), ["user:"]);
    assert_eq!(
        summarize(&requests[1].messages),
        ["user:", "assistant:空", "user:多字节输入 🦀"]
    );
}

#[tokio::test]
async fn take_queued_returns_input_no_run_consumed_steering_first() {
    let (session, provider, _) = setup([FauxStep::text("one"), FauxStep::text("two")]);
    session.prompt("first", &CancellationToken::new()).await.unwrap();
    session.queues().follow_up("later");
    session.queues().steer("now");
    session.queues().steer("also now");
    assert_eq!(
        session.take_queued().unwrap(),
        [
            Input::User("now".into()),
            Input::User("also now".into()),
            Input::User("later".into())
        ]
    );
    assert!(session.take_queued().unwrap().is_empty());
    session.prompt("second", &CancellationToken::new()).await.unwrap();
    assert_eq!(
        summarize(&provider.faux.requests()[1].messages),
        ["user:first", "assistant:one", "user:second"]
    );
}

#[tokio::test]
async fn take_queued_during_a_run_is_busy_and_leaves_the_input_queued() {
    let (session, provider, _) = setup([FauxStep::Hang]);
    let cancel = CancellationToken::new();
    let mut running = Box::pin(session.prompt("first", &cancel));
    tokio::select! { () = provider.started.notified() => {}, _ = &mut running => panic!("must be running") }
    session.queues().steer("queued");
    assert_eq!(session.take_queued(), Err(SessionError::Busy));
    assert!(session.cancel());
    assert_eq!(running.await.unwrap().outcome, RunOutcome::Cancelled);
    assert_eq!(session.take_queued().unwrap(), [Input::User("queued".into())]);
}

#[tokio::test]
async fn a_running_prompt_rejects_another_prompt_without_queueing_it() {
    let (session, provider, _) = setup([FauxStep::Hang, FauxStep::text("resumed")]);
    let cancel = CancellationToken::new();
    let mut running = Box::pin(session.prompt("first", &cancel));
    tokio::select! { () = provider.started.notified() => {}, _ = &mut running => panic!("must be running") }
    let rejected = tokio::time::timeout(
        Duration::from_secs(1),
        session.prompt("must not enter history", &CancellationToken::new()),
    )
    .await
    .unwrap();
    assert!(matches!(rejected, Err(SessionError::Busy)));
    assert_eq!(provider.faux.requests().len(), 1);
    assert!(session.cancel());
    assert_eq!(running.await.unwrap().outcome, RunOutcome::Cancelled);
    assert_eq!(
        session.prompt("next", &CancellationToken::new()).await.unwrap().outcome,
        RunOutcome::Completed
    );
    assert!(
        !summarize(&provider.faux.requests()[1].messages)
            .iter()
            .any(|text| text.contains("must not enter history"))
    );
}

#[tokio::test]
async fn abort_does_not_cancel_the_callers_token_or_the_next_prompt() {
    let (session, provider, _) = setup([FauxStep::Hang, FauxStep::text("next answer")]);
    let parent = CancellationToken::new();
    let mut running = Box::pin(session.prompt("first", &parent));
    tokio::select! { () = provider.started.notified() => {}, _ = &mut running => panic!("must be running") }
    assert!(session.cancel());
    assert_eq!(running.await.unwrap().outcome, RunOutcome::Cancelled);
    assert!(!parent.is_cancelled());
    let next = session.prompt("second", &parent).await.unwrap();
    assert_eq!(
        (next.outcome, next.final_text.as_str()),
        (RunOutcome::Completed, "next answer")
    );
    assert!(!session.cancel());
}

#[tokio::test]
async fn aborting_an_idle_session_has_no_effect_on_the_next_prompt() {
    let (session, _, _) = setup([FauxStep::text("done")]);
    assert!(!session.cancel());
    assert_eq!(
        session.prompt("go", &CancellationToken::new()).await.unwrap().outcome,
        RunOutcome::Completed
    );
}

#[tokio::test]
async fn cancelling_the_external_token_only_ends_its_prompt() {
    let (session, provider, _) = setup([FauxStep::Hang, FauxStep::text("done")]);
    let parent = CancellationToken::new();
    let mut running = Box::pin(session.prompt("first", &parent));
    tokio::select! { () = provider.started.notified() => {}, _ = &mut running => panic!("must be running") }
    parent.cancel();
    assert_eq!(running.await.unwrap().outcome, RunOutcome::Cancelled);
    assert_eq!(
        session.prompt("next", &CancellationToken::new()).await.unwrap().outcome,
        RunOutcome::Completed
    );
}

#[tokio::test]
async fn dropping_a_prompt_cancels_the_provider_and_returns_ownership() {
    let (session, provider, _) = setup([FauxStep::Hang, FauxStep::text("done")]);
    let cancel = CancellationToken::new();
    let mut running = Box::pin(session.prompt("first", &cancel));
    tokio::select! { () = provider.started.notified() => {}, _ = &mut running => panic!("must be running") }
    drop(running);
    assert!(provider.token.lock().unwrap().as_ref().unwrap().is_cancelled());
    assert!(
        !session.cancel(),
        "the dropped future must release the conversation immediately"
    );
    assert_eq!(
        session.prompt("next", &CancellationToken::new()).await.unwrap().outcome,
        RunOutcome::Completed
    );
}

#[tokio::test]
async fn panic_during_prompt_conversion_returns_ownership() {
    struct PanicPrompt;
    impl From<PanicPrompt> for Input {
        fn from(_: PanicPrompt) -> Self {
            panic!("host prompt bug")
        }
    }
    let (session, provider, _) = setup([FauxStep::text("done")]);
    let panicked = AssertUnwindSafe(session.prompt(PanicPrompt, &CancellationToken::new()))
        .catch_unwind()
        .await;
    assert!(panicked.is_err());
    assert!(!session.cancel());
    assert!(provider.faux.requests().is_empty());
    assert_eq!(
        session.prompt("next", &CancellationToken::new()).await.unwrap().outcome,
        RunOutcome::Completed
    );
}

#[tokio::test]
async fn provider_failures_remain_reports_and_do_not_make_the_session_busy() {
    let (session, _, _) = setup([FauxStep::Fail("bad request".into()), FauxStep::text("done")]);
    let failed = session.prompt("first", &CancellationToken::new()).await.unwrap();
    assert_eq!(failed.outcome, RunOutcome::Failed);
    assert_eq!(
        session.prompt("next", &CancellationToken::new()).await.unwrap().outcome,
        RunOutcome::Completed
    );
}

#[tokio::test]
async fn a_run_gives_its_events_in_order_and_then_its_report() {
    let (session, _, _) = setup([FauxStep::text("done")]);

    let mut run = session.send("go").unwrap();
    let id = run.id().to_string();
    let mut events = Vec::new();
    while let Some(event) = run.next().await {
        events.push(event);
    }
    let report = run.wait().await;

    let seqs: Vec<u64> = events.iter().map(|event| event.seq).collect();
    assert_eq!(seqs, (1..=u64::try_from(events.len()).unwrap()).collect::<Vec<_>>());
    assert!(
        events
            .iter()
            .all(|event| event.run_id == id && event.session_id == session.id())
    );
    let kinds: Vec<&str> = events.iter().map(|event| event.event.kind()).collect();
    assert_eq!((kinds[0], kinds[kinds.len() - 1]), ("run_start", "run_end"));
    assert!(kinds.contains(&"message_update"));
    let Some(EventKind::RunEnd { report: reported }) = events.last().map(|event| &event.event) else {
        panic!("the last event reports the run");
    };
    assert_eq!(**reported, report);
}

#[tokio::test]
async fn a_run_makes_no_request_until_it_is_read_and_holds_the_session_meanwhile() {
    let (session, provider, _) = setup([FauxStep::text("done")]);

    let run = session.send("go").unwrap();
    tokio::task::yield_now().await;
    assert!(provider.faux.requests().is_empty());
    assert!(matches!(session.send("second"), Err(SessionError::Busy)));
    let report = run.wait().await;

    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(provider.faux.requests().len(), 1);
    assert!(!session.cancel(), "the session is free once its run has ended");
}

/// Read `run` until its events end.
async fn read_all(run: &mut Run) {
    while run.next().await.is_some() {}
}

#[tokio::test]
async fn dropping_a_run_cancels_it_and_frees_the_session() {
    let (session, provider, _) = setup([FauxStep::Hang, FauxStep::text("done")]);
    let mut run = session.send("first").unwrap();
    tokio::select! { () = provider.started.notified() => {}, () = read_all(&mut run) => panic!("must be running") }

    drop(run);

    assert!(provider.token.lock().unwrap().as_ref().unwrap().is_cancelled());
    assert!(!session.cancel(), "a dropped run releases the conversation at once");
    assert_eq!(
        session.send("next").unwrap().wait().await.outcome,
        RunOutcome::Completed
    );
}

#[tokio::test]
async fn one_handle_cancels_the_run_that_another_reads() {
    let (session, provider, _) = setup([FauxStep::Hang]);
    let other = Session::clone(&session);
    let mut run = session.send("first").unwrap();
    tokio::select! { () = provider.started.notified() => {}, () = read_all(&mut run) => panic!("must be running") }

    assert!(matches!(other.send("second"), Err(SessionError::Busy)));
    assert!(other.cancel());

    assert_eq!(run.wait().await.outcome, RunOutcome::Cancelled);
    assert_eq!(provider.faux.requests().len(), 1);
}

#[tokio::test]
async fn a_run_goes_on_without_the_handle_that_started_it() {
    let (session, provider, _) = setup([FauxStep::text("done")]);
    let run = session.send("go").unwrap();

    drop(session);

    assert_eq!(run.wait().await.outcome, RunOutcome::Completed);
    assert_eq!(provider.faux.requests().len(), 1);
}

#[tokio::test]
async fn switching_models_preserves_history_and_uses_the_new_provider() {
    let (session, first, _) = setup([FauxStep::text("one")]);
    session.prompt("first", &CancellationToken::new()).await.unwrap();
    let next = Arc::new(FauxProvider::new([FauxStep::text("two")]));
    session
        .set_model(ModelRef {
            provider: next.clone(),
            model: "next-model".into(),
        })
        .unwrap();
    let report = session.prompt("second", &CancellationToken::new()).await.unwrap();
    assert_eq!(report.final_text, "two");
    assert_eq!(first.faux.requests().len(), 1);
    let requests = next.requests();
    assert_eq!(requests[0].model, "next-model");
    assert_eq!(
        summarize(&requests[0].messages),
        ["user:first", "assistant:one", "user:second"]
    );
}

#[tokio::test]
async fn the_reasoning_level_changes_between_prompts_and_is_recorded_for_the_audit() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("session.jsonl");
    let provider = Arc::new(FauxProvider::new([
        FauxStep::text("one"),
        FauxStep::text("two"),
        FauxStep::text("three"),
    ]));
    let mut loop_config = config(provider.clone(), vec![]);
    loop_config.reasoning = Some(Reasoning::Low);
    let (runner, _) = RunnerSetup {
        session: common::create_session_file(&file, header()).await.unwrap(),
        ..RunnerSetup::new(loop_config)
    }
    .build();
    let session = runner.into_session();

    session.prompt("first", &CancellationToken::new()).await.unwrap();
    session.set_reasoning(Some(Reasoning::High)).unwrap();
    session.prompt("second", &CancellationToken::new()).await.unwrap();
    session.set_reasoning(None).unwrap();
    session.prompt("third", &CancellationToken::new()).await.unwrap();

    let sent: Vec<Option<Reasoning>> = provider.requests().iter().map(|request| request.reasoning).collect();
    assert_eq!(sent, [Some(Reasoning::Low), Some(Reasoning::High), None]);
    // Each change is a new request configuration, and every request rebuilds as sent.
    let store = common::read_session_file(&file).unwrap();
    let ids: Vec<String> = store
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            Entry::RequestStarted(started) => Some(started.id.clone()),
            _ => None,
        })
        .collect();
    let replayed: Vec<Request> = ids.iter().map(|id| store.replay_request(id).unwrap().1).collect();
    assert_eq!(replayed, provider.requests());
    let configs = store
        .entries()
        .iter()
        .filter(|entry| matches!(entry, Entry::RequestConfig(_)))
        .count();
    assert_eq!(configs, 3);
}

#[tokio::test]
async fn a_running_session_rejects_a_reasoning_change() {
    let (session, provider, _) = setup([FauxStep::Hang, FauxStep::text("after")]);
    let cancel = CancellationToken::new();
    let mut running = Box::pin(session.prompt("first", &cancel));
    tokio::select! { () = provider.started.notified() => {}, _ = &mut running => panic!("must be running") }
    assert_eq!(session.set_reasoning(Some(Reasoning::High)), Err(SessionError::Busy));
    session.cancel();
    running.await.unwrap();
    session.prompt("second", &CancellationToken::new()).await.unwrap();
    assert_eq!(provider.faux.requests()[1].reasoning, None);
}

#[tokio::test]
async fn a_session_is_named_while_idle_and_never_during_a_run() {
    let (session, provider, _) = setup([FauxStep::Hang, FauxStep::text("after")]);
    let name = |text| SessionName::new(text).unwrap();
    session.set_name(name("triage")).await.unwrap();
    let cancel = CancellationToken::new();
    let mut running = Box::pin(session.prompt("first", &cancel));
    tokio::select! { () = provider.started.notified() => {}, _ = &mut running => panic!("must be running") }
    assert_eq!(session.set_name(name("renamed")).await, Err(SessionError::Busy));
    session.cancel();
    running.await.unwrap();
    let named = session
        .with_transcript(|store| store.session_name().map(|name| name.as_str().to_string()))
        .unwrap();
    assert_eq!(named.as_deref(), Some("triage"));
    // The name is the session's, not the conversation's: the model never sees it.
    session.prompt("second", &CancellationToken::new()).await.unwrap();
    assert!(
        !summarize(&provider.faux.requests()[1].messages)
            .iter()
            .any(|text| text.contains("triage"))
    );
}

#[tokio::test]
async fn a_running_session_rejects_model_changes_without_changing_the_provider() {
    let (session, provider, _) = setup([FauxStep::Hang, FauxStep::text("old provider")]);
    let cancel = CancellationToken::new();
    let mut running = Box::pin(session.prompt("first", &cancel));
    tokio::select! { () = provider.started.notified() => {}, _ = &mut running => panic!("must be running") }
    let next = Arc::new(FauxProvider::new([FauxStep::text("must not run")]));
    assert_eq!(
        session.set_model(ModelRef {
            provider: next.clone(),
            model: "next".into()
        }),
        Err(SessionError::Busy)
    );
    session.cancel();
    running.await.unwrap();
    assert_eq!(
        session
            .prompt("second", &CancellationToken::new())
            .await
            .unwrap()
            .final_text,
        "old provider"
    );
    assert!(next.requests().is_empty());
}

#[tokio::test]
async fn an_invalid_model_does_not_replace_the_current_configuration() {
    let (session, provider, _) = setup([FauxStep::text("done")]);
    let next = Arc::new(FauxProvider::new([]));
    for invalid in ["", " \t　"] {
        assert_eq!(
            session.set_model(ModelRef {
                provider: next.clone(),
                model: invalid.into()
            }),
            Err(SessionError::InvalidModel)
        );
    }
    session.prompt("go", &CancellationToken::new()).await.unwrap();
    assert_eq!(provider.faux.requests()[0].model, "test-model");
    assert!(next.requests().is_empty());
}

struct GatedTool {
    started: Notify,
    release: Notify,
}
#[async_trait]
impl Tool for GatedTool {
    fn name(&self) -> &str {
        "gate"
    }
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "gate".into(),
            description: "test gate".into(),
            parameters: json!({"type": "object"}),
        }
    }
    async fn execute(&self, _args: Value, context: &ToolContext) -> Result<ToolOutput, String> {
        self.started.notify_one();
        tokio::select! {
            () = self.release.notified() => Ok(ToolOutput::ok("done")),
            () = context.cancel.cancelled() => Err("cancelled".into()),
        }
    }
}

#[tokio::test]
async fn steering_and_follow_ups_reach_the_model_at_their_defined_boundaries() {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c", "gate", json!({})),
        FauxStep::text("steered"),
        FauxStep::text("followed"),
    ]));
    let gate = Arc::new(GatedTool {
        started: Notify::new(),
        release: Notify::new(),
    });
    let (runner, _) = RunnerSetup::new(config(provider.clone(), vec![gate.clone()])).build();
    let session = runner.into_session();
    let cancel = CancellationToken::new();
    let mut running = Box::pin(session.prompt("go", &cancel));
    tokio::select! { () = gate.started.notified() => {}, _ = &mut running => panic!("tool must wait") }
    session.queues().steer("steering");
    session.queues().follow_up("follow-up");
    gate.release.notify_one();
    let report = running.await.unwrap();
    assert_eq!(
        (report.outcome, report.model_requests, report.final_text.as_str()),
        (RunOutcome::Completed, 3, "followed")
    );
    let requests = provider.requests();
    assert_eq!(
        summarize(&requests[1].messages),
        ["user:go", "assistant:calls[c]", "tool:c=done", "user:steering"]
    );
    assert!(summarize(&requests[2].messages).last().unwrap().ends_with("follow-up"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_callers_share_one_running_prompt() {
    const CALLERS: usize = 6;
    let (session, provider, _) = setup([FauxStep::Hang]);
    let session = Arc::new(session);
    let barrier = Arc::new(tokio::sync::Barrier::new(CALLERS + 1));
    let rejected = Arc::new(AtomicUsize::new(0));
    let all_rejected = Arc::new(Notify::new());
    let mut tasks = Vec::new();
    for index in 0..CALLERS {
        let session = session.clone();
        let barrier = barrier.clone();
        let rejected = rejected.clone();
        let all_rejected = all_rejected.clone();
        // Separate frontend tasks must be independently scheduled on the executor.
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            let result = session
                .prompt(format!("caller {index}"), &CancellationToken::new())
                .await;
            if matches!(&result, Err(SessionError::Busy)) && rejected.fetch_add(1, Ordering::SeqCst) + 1 == CALLERS - 1
            {
                all_rejected.notify_one();
            }
            result
        }));
    }
    barrier.wait().await;
    tokio::time::timeout(Duration::from_secs(2), all_rejected.notified())
        .await
        .unwrap();
    // Busy can be observed before the winning caller starts its provider request.
    tokio::time::timeout(Duration::from_secs(2), provider.started.notified())
        .await
        .unwrap();
    assert!(session.cancel());
    let mut completed = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(report) => {
                assert_eq!(report.outcome, RunOutcome::Cancelled);
                completed += 1;
            }
            Err(error) => assert_eq!(error, SessionError::Busy),
        }
    }
    assert_eq!(completed, 1);
    assert_eq!(provider.faux.requests().len(), 1);
}

struct Ask;
#[async_trait]
impl ToolPolicy for Ask {
    async fn check(&self, _call: &zx_agent::ai::ToolCall, _ctx: &PolicyContext) -> PolicyDecision {
        PolicyDecision::Ask("review".into())
    }
}
struct Approver(AtomicUsize);
#[async_trait]
impl common::Approver for Approver {
    async fn confirm(&self, _request: &common::Asked) -> Option<ApprovalDecision> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Some(ApprovalDecision::AllowSession)
    }
}

#[tokio::test]
async fn model_switching_retains_approval_grants() {
    let steps = || {
        [
            FauxStep::tool_call("c", "echo", json!({"text": "same"})),
            FauxStep::text("done"),
        ]
    };
    let first = Arc::new(FauxProvider::new(steps()));
    let next = Arc::new(FauxProvider::new(steps()));
    let host = Arc::new(Approver(AtomicUsize::new(0)));
    let mut config = config(first, vec![Arc::new(common::EchoTool::new("echo"))]);
    config.policies.push("ask", Arc::new(Ask));
    let (runner, _) = RunnerSetup {
        approver: Some(host.clone()),
        ..RunnerSetup::new(config)
    }
    .build();
    let session = runner.into_session();
    session.prompt("one", &CancellationToken::new()).await.unwrap();
    session
        .set_model(ModelRef {
            provider: next,
            model: "next".into(),
        })
        .unwrap();
    session.prompt("two", &CancellationToken::new()).await.unwrap();
    assert_eq!(host.0.load(Ordering::SeqCst), 1);
}

/// A session whose every call of `echo` asks for approval and waits `timeout` for the
/// answer, and that tool.
fn asking_session(provider: Arc<FauxProvider>, timeout: Duration) -> (Driven, Arc<common::EchoTool>) {
    let tool = Arc::new(common::EchoTool::new("echo"));
    let mut config = config(provider, vec![tool.clone()]);
    config.policies.push("ask", Arc::new(Ask));
    config.approvals = ApprovalMode::Wait(timeout);
    let (runner, _) = RunnerSetup::new(config).build();
    (runner.into_session(), tool)
}

#[tokio::test]
async fn an_approval_is_answered_through_the_session_with_the_id_its_event_gives() {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c", "echo", json!({"text": "hi"})),
        FauxStep::text("done"),
    ]));
    let (session, tool) = asking_session(provider, Duration::from_secs(60));

    let mut run = session.send("go").unwrap();
    let mut asked = Vec::new();
    while let Some(event) = run.next().await {
        if let EventKind::ApprovalRequested {
            approval_id,
            tool_call_id,
            tool_name,
            args,
            reason,
        } = event.event
        {
            assert_eq!(
                session.resolve_approval("another", ApprovalDecision::AllowOnce),
                Err(SessionError::UnknownApproval)
            );
            session
                .resolve_approval(&approval_id, ApprovalDecision::AllowOnce)
                .unwrap();
            assert_eq!(
                session.resolve_approval(&approval_id, ApprovalDecision::AllowOnce),
                Err(SessionError::UnknownApproval),
                "a request is answered once"
            );
            asked.push((tool_call_id, tool_name, args, reason));
        }
    }

    assert_eq!(run.wait().await.outcome, RunOutcome::Completed);
    assert_eq!(
        asked,
        [(
            "c".to_string(),
            "echo".to_string(),
            json!({"text": "hi"}),
            "review".to_string()
        )]
    );
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn an_answer_that_comes_after_the_request_timed_out_reaches_no_call() {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c", "echo", json!({"text": "hi"})),
        FauxStep::text("went on"),
    ]));
    let (session, tool) = asking_session(provider, Duration::ZERO);

    let mut run = session.send("go").unwrap();
    let mut requests = Vec::new();
    while let Some(event) = run.next().await {
        if let EventKind::ApprovalRequested { approval_id, .. } = event.event {
            requests.push(approval_id);
        }
    }
    let report = run.wait().await;

    assert_eq!((report.outcome, report.denied_calls), (RunOutcome::Completed, 1));
    assert_eq!(requests.len(), 1);
    assert_eq!(
        session.resolve_approval(&requests[0], ApprovalDecision::AllowOnce),
        Err(SessionError::UnknownApproval)
    );
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn switching_models_keeps_the_session_header_and_append_only_history() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("session.jsonl");
    let provider = Arc::new(FauxProvider::new([FauxStep::text("one")]));
    let store = common::create_session_file(&file, header()).await.unwrap();
    let (runner, _) = RunnerSetup {
        session: store,
        ..RunnerSetup::new(config(provider, vec![]))
    }
    .build();
    let session = runner.into_session();
    session.prompt("first", &CancellationToken::new()).await.unwrap();
    let before = std::fs::read(&file).unwrap();
    let next = Arc::new(FauxProvider::new([FauxStep::text("two")]));
    session
        .set_model(ModelRef {
            provider: next,
            model: "new-model".into(),
        })
        .unwrap();
    assert_eq!(
        std::fs::read(&file).unwrap(),
        before,
        "a model change alone must not rewrite history"
    );
    session.prompt("second", &CancellationToken::new()).await.unwrap();
    assert!(std::fs::read(&file).unwrap().starts_with(&before));
    let reopened = common::read_session_file(&file).unwrap();
    assert_eq!(reopened.header().model, "test-model");
    assert!(
        matches!(reopened.messages().last(), Some(zx_agent::ai::Message::Assistant(message)) if message.model == "new-model")
    );
}

#[tokio::test]
async fn the_reader_gets_a_message_only_once_it_is_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("session.jsonl");
    let store = common::create_session_file(&file, header()).await.unwrap();
    let provider = Arc::new(FauxProvider::new([FauxStep::text("done")]));
    let (runner, _) = RunnerSetup {
        session: store,
        ..RunnerSetup::new(config(provider, vec![]))
    }
    .build();
    let session = runner.into_session();

    let mut run = session.send("go").unwrap();
    let mut observed = 0;
    while let Some(event) = run.next().await {
        if let EventKind::MessageEnd { message, .. } = &event.event {
            let stored = common::read_session_file(&file).unwrap();
            assert!(stored.messages().contains(message), "{message:?}");
            observed += 1;
        }
    }

    assert_eq!(observed, 2);
    assert_eq!(common::read_session_file(&file).unwrap().messages().len(), 2);
}

/// Refuses every record, like a database that no longer accepts writes.
struct RefusingLog;

#[async_trait]
impl zx_agent::transcript::SessionLog for RefusingLog {
    async fn append(&mut self, _record: &zx_agent::transcript::LogRecord<'_>) -> anyhow::Result<()> {
        anyhow::bail!("the database is read-only")
    }
}

#[tokio::test]
async fn a_session_reports_why_it_stopped_writing() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("never sent")]));
    let (runner, _) = RunnerSetup {
        session: Transcript::with_log(header(), Box::new(RefusingLog)),
        ..RunnerSetup::new(config(provider, vec![]))
    }
    .build();
    let session = runner.into_session();
    let writes_stopped = || {
        session
            .with_transcript(|store| store.writes_stopped().cloned())
            .unwrap()
    };
    assert_eq!(writes_stopped(), None);

    let report = session.prompt("go", &CancellationToken::new()).await.unwrap();

    assert_eq!(report.outcome, RunOutcome::Failed);
    assert!(
        matches!(
            writes_stopped(),
            Some(zx_agent::transcript::WritesStopped::Failed(error)) if error.contains("the database is read-only")
        ),
        "{:?}",
        writes_stopped()
    );
}
