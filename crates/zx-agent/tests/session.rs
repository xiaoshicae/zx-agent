//! Shared conversation ownership, cancellation, the run a prompt returns, and model
//! switching.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt;
use serde_json::{Value, json};
use tokio::sync::Notify;

use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::{ProviderErrorKind, Reasoning, Request, ToolDef};
use zx_agent::events::{Event, EventKind, RunOutcome};
use zx_agent::hooks::{Hook, HookContext, HookResult};
use zx_agent::input::{Input, PendingInput};
use zx_agent::policy::{ApprovalDecision, ApprovalMode, PolicyContext, PolicyDecision, ToolPolicy};
use zx_agent::session::SessionError;
use zx_agent::tools::{Tool, ToolContext, ToolOutput};
use zx_agent::transcript::{Entry, SessionName};
use zx_agent::{Run, Session};
use zx_testing::{
    Collect, Driven, NotifyingProvider, RunnerSetup, Start, config, header, summarize, take_queued_inputs,
};

async fn setup(steps: impl IntoIterator<Item = FauxStep>) -> (Driven, Arc<NotifyingProvider>, Arc<Collect>) {
    let provider = NotifyingProvider::new(FauxProvider::new(steps));
    let (runner, events) = RunnerSetup::new(config(provider.clone(), vec![])).build().await;
    (runner, provider, events)
}

#[tokio::test]
async fn sequential_prompts_keep_history_and_each_reports_its_own_answer() {
    let (session, provider, _) = setup([FauxStep::text("one"), FauxStep::text("two")]).await;
    let first = session.prompt("first").await.unwrap();
    let second = session.prompt("second").await.unwrap();
    assert_eq!((first.final_text.as_str(), second.final_text.as_str()), ("one", "two"));
    assert_eq!(
        summarize(&provider.requests()[1].messages),
        ["user:first", "assistant:one", "user:second"]
    );
    assert_eq!(second.usage.total_tokens, 15);
}

#[tokio::test]
async fn sdk_prompts_preserve_empty_and_unicode_input() {
    let (session, provider, _) = setup([FauxStep::text("空"), FauxStep::text("你好")]).await;
    session.prompt("").await.unwrap();
    let report = session.prompt("多字节输入 🦀").await.unwrap();
    assert_eq!(report.final_text, "你好");
    let requests = provider.requests();
    assert_eq!(summarize(&requests[0].messages), ["user:"]);
    assert_eq!(
        summarize(&requests[1].messages),
        ["user:", "assistant:空", "user:多字节输入 🦀"]
    );
}

#[tokio::test]
async fn take_queued_returns_input_no_run_consumed_steering_first() {
    let (session, provider, _) = setup([FauxStep::text("one"), FauxStep::text("two")]).await;
    session.prompt("first").await.unwrap();
    session.queues().follow_up("later");
    session.queues().steer("now");
    session.queues().steer("also now");
    assert_eq!(
        take_queued_inputs(&session).unwrap(),
        [Input::from("now"), Input::from("also now"), Input::from("later")]
    );
    assert!(take_queued_inputs(&session).unwrap().is_empty());
    session.prompt("second").await.unwrap();
    assert_eq!(
        summarize(&provider.requests()[1].messages),
        ["user:first", "assistant:one", "user:second"]
    );
}

#[tokio::test]
async fn take_queued_during_a_run_is_busy_and_leaves_the_input_queued() {
    let (session, provider, _) = setup([FauxStep::Hang]).await;
    let mut running = Box::pin(session.prompt("first"));
    tokio::select! { () = provider.started.notified() => {}, _ = &mut running => panic!("must be running") }
    session.queues().steer("queued");
    assert_eq!(take_queued_inputs(&session), Err(SessionError::Busy));
    assert!(session.cancel());
    assert_eq!(running.await.unwrap().outcome, RunOutcome::Cancelled);
    assert_eq!(take_queued_inputs(&session).unwrap(), [Input::from("queued")]);
}

#[tokio::test]
async fn a_running_prompt_rejects_another_prompt_without_queueing_it() {
    let (session, provider, _) = setup([FauxStep::Hang, FauxStep::text("resumed")]).await;
    let mut running = Box::pin(session.prompt("first"));
    tokio::select! { () = provider.started.notified() => {}, _ = &mut running => panic!("must be running") }
    let rejected = tokio::time::timeout(Duration::from_secs(1), session.prompt("must not enter history"))
        .await
        .unwrap();
    assert!(matches!(rejected, Err(SessionError::Busy)));
    assert_eq!(provider.requests().len(), 1);
    assert!(session.cancel());
    assert_eq!(running.await.unwrap().outcome, RunOutcome::Cancelled);
    assert_eq!(session.prompt("next").await.unwrap().outcome, RunOutcome::Completed);
    assert!(
        !summarize(&provider.requests()[1].messages)
            .iter()
            .any(|text| text.contains("must not enter history"))
    );
}

#[tokio::test]
async fn cancelling_a_run_ends_only_that_run() {
    let (session, provider, _) = setup([FauxStep::Hang, FauxStep::text("next answer")]).await;
    let mut running = Box::pin(session.prompt("first"));
    tokio::select! { () = provider.started.notified() => {}, _ = &mut running => panic!("must be running") }
    assert!(session.cancel());
    assert_eq!(running.await.unwrap().outcome, RunOutcome::Cancelled);
    let next = session.prompt("second").await.unwrap();
    assert_eq!(
        (next.outcome, next.final_text.as_str()),
        (RunOutcome::Completed, "next answer")
    );
    assert!(!session.cancel());
}

#[tokio::test]
async fn aborting_an_idle_session_has_no_effect_on_the_next_prompt() {
    let (session, _, _) = setup([FauxStep::text("done")]).await;
    assert!(!session.cancel());
    assert_eq!(session.prompt("go").await.unwrap().outcome, RunOutcome::Completed);
}

#[tokio::test]
async fn reading_the_session_leaves_nothing_to_cancel() {
    let (session, _, _) = setup([]).await;

    let cancelled = session.with_transcript(|_| session.cancel()).unwrap();

    assert!(!cancelled);
}

#[tokio::test]
async fn dropping_a_prompt_cancels_the_provider_and_returns_ownership() {
    let (session, provider, _) = setup([FauxStep::Hang, FauxStep::text("done")]).await;
    let mut running = Box::pin(session.prompt("first"));
    tokio::select! { () = provider.started.notified() => {}, _ = &mut running => panic!("must be running") }
    drop(running);
    assert!(provider.token.lock().unwrap().as_ref().unwrap().is_cancelled());
    assert!(
        !session.cancel(),
        "the dropped future must release the conversation immediately"
    );
    assert_eq!(session.prompt("next").await.unwrap().outcome, RunOutcome::Completed);
}

#[tokio::test]
async fn panic_during_prompt_conversion_returns_ownership() {
    struct PanicPrompt;
    impl From<PanicPrompt> for Input {
        fn from(_: PanicPrompt) -> Self {
            panic!("host prompt bug")
        }
    }
    let (session, provider, _) = setup([FauxStep::text("done")]).await;
    let panicked = AssertUnwindSafe(session.prompt(PanicPrompt)).catch_unwind().await;
    assert!(panicked.is_err());
    assert!(!session.cancel());
    assert!(provider.requests().is_empty());
    assert_eq!(session.prompt("next").await.unwrap().outcome, RunOutcome::Completed);
}

#[tokio::test]
async fn provider_failures_remain_reports_and_do_not_make_the_session_busy() {
    let (session, _, _) = setup([
        FauxStep::fail(ProviderErrorKind::Other, "bad request"),
        FauxStep::text("done"),
    ])
    .await;
    let failed = session.prompt("first").await.unwrap();
    assert_eq!(failed.outcome, RunOutcome::Failed);
    assert_eq!(session.prompt("next").await.unwrap().outcome, RunOutcome::Completed);
}

#[tokio::test]
async fn a_run_gives_its_events_in_order_and_then_its_report() {
    let (session, _, _) = setup([FauxStep::text("done")]).await;

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
    let (session, provider, _) = setup([FauxStep::text("done")]).await;

    let run = session.send("go").unwrap();
    tokio::task::yield_now().await;
    assert!(provider.requests().is_empty());
    assert!(matches!(session.send("second"), Err(SessionError::Busy)));
    let report = run.wait().await;

    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(provider.requests().len(), 1);
    assert!(!session.cancel(), "the session is free once its run has ended");
}

/// Read `run` until its events end.
async fn read_all(run: &mut Run) {
    while run.next().await.is_some() {}
}

/// A hold of the host's that says when it has ended.
struct Hold(Arc<AtomicBool>);

impl Drop for Hold {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn a_prompt_s_hold_ends_once_the_session_has_recorded_the_prompt() {
    let (session, provider, _) = setup([FauxStep::Hang]).await;
    let ended = Arc::new(AtomicBool::new(false));
    let mut run = session
        .send(PendingInput::held("go", Hold(Arc::clone(&ended))))
        .unwrap();
    tokio::task::yield_now().await;
    assert!(!ended.load(Ordering::SeqCst), "a run not read yet has recorded nothing");

    // The prompt is recorded before the first request.
    tokio::select! { () = provider.started.notified() => {}, () = read_all(&mut run) => panic!("must be running") }
    assert!(ended.load(Ordering::SeqCst));
    assert_eq!(summarize(&provider.requests()[0].messages), ["user:go"]);
}

#[tokio::test]
async fn a_prompt_s_hold_ends_with_a_run_that_never_recorded_it() {
    let (session, provider, _) = setup([FauxStep::text("done")]).await;
    let ended = Arc::new(AtomicBool::new(false));
    let run = session
        .send(PendingInput::held("go", Hold(Arc::clone(&ended))))
        .unwrap();

    drop(run);

    assert!(ended.load(Ordering::SeqCst));
    assert!(provider.requests().is_empty());
}

#[tokio::test]
async fn dropping_a_run_cancels_it_and_frees_the_session() {
    let (session, provider, _) = setup([FauxStep::Hang, FauxStep::text("done")]).await;
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
    let (session, provider, _) = setup([FauxStep::Hang]).await;
    let other = Session::clone(&session);
    let mut run = session.send("first").unwrap();
    tokio::select! { () = provider.started.notified() => {}, () = read_all(&mut run) => panic!("must be running") }

    assert!(matches!(other.send("second"), Err(SessionError::Busy)));
    assert!(other.cancel());

    assert_eq!(run.wait().await.outcome, RunOutcome::Cancelled);
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn a_run_goes_on_without_the_handle_that_started_it() {
    let (session, provider, _) = setup([FauxStep::text("done")]).await;
    let run = session.send("go").unwrap();

    drop(session);

    assert_eq!(run.wait().await.outcome, RunOutcome::Completed);
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn switching_models_preserves_history_and_uses_the_new_provider() {
    let (session, first, _) = setup([FauxStep::text("one")]).await;
    session.prompt("first").await.unwrap();
    let next = Arc::new(FauxProvider::new([FauxStep::text("two")]));
    session.set_model(next.clone(), "next-model").unwrap();
    let report = session.prompt("second").await.unwrap();
    assert_eq!(report.final_text, "two");
    assert_eq!(first.requests().len(), 1);
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
    let (session, _) = RunnerSetup {
        session: zx_testing::create_session_file(&file, header()).await.unwrap().into(),
        ..RunnerSetup::new(loop_config)
    }
    .build()
    .await;

    session.prompt("first").await.unwrap();
    session.set_reasoning(Some(Reasoning::High)).unwrap();
    session.prompt("second").await.unwrap();
    session.set_reasoning(None).unwrap();
    session.prompt("third").await.unwrap();

    let sent: Vec<Option<Reasoning>> = provider.requests().iter().map(|request| request.reasoning).collect();
    assert_eq!(sent, [Some(Reasoning::Low), Some(Reasoning::High), None]);
    // Each change is a new request configuration, and every request rebuilds as sent.
    let transcript = zx_testing::read_session_file(&file).unwrap();
    let ids: Vec<String> = transcript
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            Entry::RequestStarted(started) => Some(started.id.clone()),
            _ => None,
        })
        .collect();
    let replayed: Vec<Request> = ids.iter().map(|id| transcript.replay_request(id).unwrap().1).collect();
    assert_eq!(replayed, provider.requests());
    let configs = transcript
        .entries()
        .iter()
        .filter(|entry| matches!(entry, Entry::RequestConfig(_)))
        .count();
    assert_eq!(configs, 3);
}

#[tokio::test]
async fn a_running_session_rejects_a_reasoning_change() {
    let (session, provider, _) = setup([FauxStep::Hang, FauxStep::text("after")]).await;
    let mut running = Box::pin(session.prompt("first"));
    tokio::select! { () = provider.started.notified() => {}, _ = &mut running => panic!("must be running") }
    assert_eq!(session.set_reasoning(Some(Reasoning::High)), Err(SessionError::Busy));
    session.cancel();
    running.await.unwrap();
    session.prompt("second").await.unwrap();
    assert_eq!(provider.requests()[1].reasoning, None);
}

#[tokio::test]
async fn a_session_is_named_while_idle_and_never_during_a_run() {
    let (session, provider, _) = setup([FauxStep::Hang, FauxStep::text("after")]).await;
    let name = |text| SessionName::new(text).unwrap();
    session.set_name(name("triage")).await.unwrap();
    let mut running = Box::pin(session.prompt("first"));
    tokio::select! { () = provider.started.notified() => {}, _ = &mut running => panic!("must be running") }
    assert_eq!(session.set_name(name("renamed")).await, Err(SessionError::Busy));
    session.cancel();
    running.await.unwrap();
    let named = session
        .with_transcript(|transcript| transcript.session_name().map(|name| name.as_str().to_string()))
        .unwrap();
    assert_eq!(named.as_deref(), Some("triage"));
    // The name is the session's, not the conversation's: the model never sees it.
    session.prompt("second").await.unwrap();
    assert!(
        !summarize(&provider.requests()[1].messages)
            .iter()
            .any(|text| text.contains("triage"))
    );
}

#[tokio::test]
async fn a_running_session_rejects_model_changes_without_changing_the_provider() {
    let (session, provider, _) = setup([FauxStep::Hang, FauxStep::text("old provider")]).await;
    let mut running = Box::pin(session.prompt("first"));
    tokio::select! { () = provider.started.notified() => {}, _ = &mut running => panic!("must be running") }
    let next = Arc::new(FauxProvider::new([FauxStep::text("must not run")]));
    assert_eq!(session.set_model(next.clone(), "next"), Err(SessionError::Busy));
    session.cancel();
    running.await.unwrap();
    assert_eq!(session.prompt("second").await.unwrap().final_text, "old provider");
    assert!(next.requests().is_empty());
}

#[tokio::test]
async fn an_invalid_model_does_not_replace_the_current_configuration() {
    let (session, provider, _) = setup([FauxStep::text("done")]).await;
    let next = Arc::new(FauxProvider::new([]));
    for invalid in ["", " \t　"] {
        assert_eq!(
            session.set_model(next.clone(), invalid),
            Err(SessionError::InvalidModel)
        );
    }
    session.prompt("go").await.unwrap();
    assert_eq!(provider.requests()[0].model, "test-model");
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
    let (session, _) = RunnerSetup::new(config(provider.clone(), vec![gate.clone()]))
        .build()
        .await;
    let mut running = Box::pin(session.prompt("go"));
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
    let (session, provider, _) = setup([FauxStep::Hang]).await;
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
            let result = session.prompt(format!("caller {index}")).await;
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
    assert_eq!(provider.requests().len(), 1);
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
impl zx_testing::Approver for Approver {
    async fn confirm(&self, _request: &zx_testing::Asked) -> Option<ApprovalDecision> {
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
    let mut config = config(first, vec![Arc::new(zx_testing::EchoTool::new("echo"))]);
    config.policies.push("ask", Arc::new(Ask));
    let (session, _) = RunnerSetup {
        approver: Some(host.clone()),
        ..RunnerSetup::new(config)
    }
    .build()
    .await;
    session.prompt("one").await.unwrap();
    session.set_model(next, "next").unwrap();
    session.prompt("two").await.unwrap();
    assert_eq!(host.0.load(Ordering::SeqCst), 1);
}

/// A session whose every call of `echo` asks for approval and waits `timeout` for the
/// answer, and that tool.
async fn asking_session(provider: Arc<FauxProvider>, timeout: Duration) -> (Driven, Arc<zx_testing::EchoTool>) {
    let tool = Arc::new(zx_testing::EchoTool::new("echo"));
    let mut config = config(provider, vec![tool.clone()]);
    config.policies.push("ask", Arc::new(Ask));
    config.approvals = ApprovalMode::Wait(timeout);
    let (runner, _) = RunnerSetup::new(config).build().await;
    (runner, tool)
}

#[tokio::test]
async fn an_approval_is_answered_through_the_session_with_the_id_its_event_gives() {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c", "echo", json!({"text": "hi"})),
        FauxStep::text("done"),
    ]));
    let (session, tool) = asking_session(provider, Duration::from_secs(60)).await;

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
    let (session, tool) = asking_session(provider, Duration::ZERO).await;

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

/// Holds the run in the `approval_requested` event until the test lets it go.
struct HoldTheRequest(Notify);

#[async_trait]
impl Hook for HoldTheRequest {
    async fn on_event(&self, event: &Event, _ctx: &HookContext) -> HookResult<()> {
        if matches!(event.event, EventKind::ApprovalRequested { .. }) {
            self.0.notified().await;
        }
        Ok(())
    }
}

#[tokio::test]
async fn without_a_person_to_ask_a_request_takes_no_answer() {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c", "echo", json!({"text": "hi"})),
        FauxStep::text("went on"),
    ]));
    let tool = Arc::new(zx_testing::EchoTool::new("echo"));
    let hold = Arc::new(HoldTheRequest(Notify::new()));
    let mut config = config(provider, vec![tool.clone()]);
    config.policies.push("ask", Arc::new(Ask));
    config.approvals = ApprovalMode::Deny;
    config.hooks.push("hold", hold.clone());
    let (session, _) = RunnerSetup::new(config).build().await;

    let mut run = session.send("go").unwrap();
    let mut answers = Vec::new();
    while let Some(event) = run.next().await {
        if let EventKind::ApprovalRequested { approval_id, .. } = event.event {
            // The run waits in the event, before the request is decided.
            answers.push(session.resolve_approval(&approval_id, ApprovalDecision::AllowOnce));
            hold.0.notify_one();
        }
    }
    let report = run.wait().await;

    assert_eq!(answers, [Err(SessionError::UnknownApproval)]);
    assert_eq!((report.outcome, report.denied_calls), (RunOutcome::Completed, 1));
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn switching_models_keeps_the_session_header_and_append_only_history() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("session.jsonl");
    let provider = Arc::new(FauxProvider::new([FauxStep::text("one")]));
    let seed = zx_testing::create_session_file(&file, header()).await.unwrap();
    let (session, _) = RunnerSetup {
        session: seed.into(),
        ..RunnerSetup::new(config(provider, vec![]))
    }
    .build()
    .await;
    session.prompt("first").await.unwrap();
    let before = std::fs::read(&file).unwrap();
    let next = Arc::new(FauxProvider::new([FauxStep::text("two")]));
    session.set_model(next, "new-model").unwrap();
    assert_eq!(
        std::fs::read(&file).unwrap(),
        before,
        "a model change alone must not rewrite history"
    );
    session.prompt("second").await.unwrap();
    assert!(std::fs::read(&file).unwrap().starts_with(&before));
    let reopened = zx_testing::read_session_file(&file).unwrap();
    assert_eq!(reopened.header().model, "test-model");
    assert!(
        matches!(reopened.messages().last(), Some(zx_agent::ai::Message::Assistant(message)) if message.model == "new-model")
    );
}

#[tokio::test]
async fn the_reader_gets_a_message_only_once_it_is_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("session.jsonl");
    let seed = zx_testing::create_session_file(&file, header()).await.unwrap();
    let provider = Arc::new(FauxProvider::new([FauxStep::text("done")]));
    let (session, _) = RunnerSetup {
        session: seed.into(),
        ..RunnerSetup::new(config(provider, vec![]))
    }
    .build()
    .await;

    let mut run = session.send("go").unwrap();
    let mut observed = 0;
    while let Some(event) = run.next().await {
        if let EventKind::MessageEnd { message, .. } = &event.event {
            let stored = zx_testing::read_session_file(&file).unwrap();
            assert!(stored.messages().contains(message), "{message:?}");
            observed += 1;
        }
    }

    assert_eq!(observed, 2);
    assert_eq!(zx_testing::read_session_file(&file).unwrap().messages().len(), 2);
}

#[tokio::test]
async fn a_session_whose_record_cannot_be_written_says_why_and_closes() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("never sent")]));
    let (session, _) = RunnerSetup {
        session: Start::with_log(zx_testing::RefusingLog),
        ..RunnerSetup::new(config(provider.clone(), vec![]))
    }
    .build()
    .await;
    assert!(
        !session.is_closed(),
        "a session that has written nothing in vain is open"
    );

    let report = session.prompt("go").await.unwrap();

    assert_eq!(report.outcome, RunOutcome::Failed);
    let error = report.error.as_deref().unwrap_or_default();
    assert!(error.contains("the database is read-only"), "{error}");
    assert!(
        provider.requests().is_empty(),
        "nothing is asked without a record of it"
    );
    // What it holds is released, and the agent opens it again from its storage.
    assert!(matches!(session.send("again"), Err(SessionError::Closed)));
}
