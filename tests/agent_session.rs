//! Shared conversation ownership, cancellation, subscriptions and model switching.
mod common;

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use common::{Collect, NotifyingProvider, RunnerSetup, config, header, summarize};
use zx_agent::AgentSession;
use zx_agent::agent::{
    AgentEvent, Event, HookContext, HookResult, Hooks, PolicyDecision, QueuedMessage, Tool, ToolContext, ToolOutput,
};
use zx_agent::agent::{Approval, ApprovalRequest, Interaction};
use zx_agent::agent_session::{AgentSessionError, ModelRef, Subscription};
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::{Reasoning, Request, ToolDef};
use zx_agent::runner::RunOutcome;
use zx_agent::session::{Entry, SessionStore};

fn setup(steps: impl IntoIterator<Item = FauxStep>) -> (AgentSession, Arc<NotifyingProvider>, Arc<Collect>) {
    let provider = NotifyingProvider::new(steps);
    let (runner, events) = RunnerSetup::new(config(provider.clone(), vec![])).build();
    (AgentSession::from(runner), provider, events)
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
            QueuedMessage::User("now".into()),
            QueuedMessage::User("also now".into()),
            QueuedMessage::User("later".into())
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
    assert_eq!(session.take_queued(), Err(AgentSessionError::Busy));
    assert!(session.abort());
    assert_eq!(running.await.unwrap().outcome, RunOutcome::Aborted);
    assert_eq!(session.take_queued().unwrap(), [QueuedMessage::User("queued".into())]);
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
    assert!(matches!(rejected, Err(AgentSessionError::Busy)));
    assert_eq!(provider.faux.requests().len(), 1);
    assert!(session.abort());
    assert_eq!(running.await.unwrap().outcome, RunOutcome::Aborted);
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
    assert!(session.abort());
    assert_eq!(running.await.unwrap().outcome, RunOutcome::Aborted);
    assert!(!parent.is_cancelled());
    let next = session.prompt("second", &parent).await.unwrap();
    assert_eq!(
        (next.outcome, next.final_text.as_str()),
        (RunOutcome::Completed, "next answer")
    );
    assert!(!session.abort());
}

#[tokio::test]
async fn aborting_an_idle_session_has_no_effect_on_the_next_prompt() {
    let (session, _, _) = setup([FauxStep::text("done")]);
    assert!(!session.abort());
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
    assert_eq!(running.await.unwrap().outcome, RunOutcome::Aborted);
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
        !session.abort(),
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
    impl From<PanicPrompt> for QueuedMessage {
        fn from(_: PanicPrompt) -> Self {
            panic!("host prompt bug")
        }
    }
    let (session, provider, _) = setup([FauxStep::text("done")]);
    let panicked = AssertUnwindSafe(session.prompt(PanicPrompt, &CancellationToken::new()))
        .catch_unwind()
        .await;
    assert!(panicked.is_err());
    assert!(!session.abort());
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
    assert_eq!(failed.outcome, RunOutcome::Error);
    assert_eq!(
        session.prompt("next", &CancellationToken::new()).await.unwrap().outcome,
        RunOutcome::Completed
    );
}

#[tokio::test]
async fn every_subscriber_receives_the_same_events_as_the_initial_sink() {
    let (session, _, initial) = setup([FauxStep::text("done")]);
    let first = Arc::new(Collect::default());
    let second = Arc::new(Collect::default());
    let _first = session.subscribe(first.clone());
    let _second = session.subscribe(second.clone());
    session.prompt("go", &CancellationToken::new()).await.unwrap();
    assert_eq!(first.events(), initial.events());
    assert_eq!(second.events(), initial.events());
    assert!(first.kinds().contains(&"message_update"));
}

#[tokio::test]
async fn dropping_a_subscription_stops_future_notifications() {
    let (session, _, initial) = setup([FauxStep::text("one"), FauxStep::text("two")]);
    let observer = Arc::new(Collect::default());
    let subscription = session.subscribe(observer.clone());
    session.prompt("first", &CancellationToken::new()).await.unwrap();
    let count = observer.events().len();
    drop(subscription);
    session.prompt("second", &CancellationToken::new()).await.unwrap();
    assert_eq!(observer.events().len(), count);
    assert!(initial.events().len() > count);
}

#[tokio::test]
async fn subscriptions_added_during_a_run_receive_future_events_without_replay() {
    let (session, provider, _) = setup([FauxStep::Hang]);
    let cancel = CancellationToken::new();
    let mut running = Box::pin(session.prompt("first", &cancel));
    tokio::select! { () = provider.started.notified() => {}, _ = &mut running => panic!("must be running") }
    let observer = Arc::new(Collect::default());
    let _subscription = session.subscribe(observer.clone());
    session.abort();
    running.await.unwrap();
    assert!(!observer.kinds().contains(&"agent_start"));
    assert!(observer.kinds().contains(&"agent_end"));
}

#[tokio::test]
async fn a_callback_can_unsubscribe_itself_and_abort_without_deadlocking() {
    let (session, provider, _) = setup([FauxStep::text("must not run")]);
    let session = Arc::new(session);
    let weak = Arc::downgrade(&session);
    let handle: Arc<Mutex<Option<Subscription>>> = Arc::new(Mutex::new(None));
    let handle_in_callback = handle.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed_calls = calls.clone();
    let callback = Arc::new(move |_: &Event| {
        observed_calls.fetch_add(1, Ordering::SeqCst);
        let subscription = handle_in_callback.lock().unwrap().take();
        drop(subscription);
        assert!(weak.upgrade().unwrap().abort());
    });
    *handle.lock().unwrap() = Some(session.subscribe(callback));
    let report = tokio::time::timeout(Duration::from_secs(1), session.prompt("go", &CancellationToken::new()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.outcome, RunOutcome::Aborted);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(provider.faux.requests().is_empty());
}

#[tokio::test]
async fn a_panicking_subscriber_is_detached_without_silencing_other_subscribers() {
    let (session, _, initial) = setup([FauxStep::text("one"), FauxStep::text("two")]);
    let calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = calls.clone();
    let _failed = session.subscribe(Arc::new(move |_: &Event| {
        callback_calls.fetch_add(1, Ordering::SeqCst);
        panic!("subscriber bug");
    }));
    let observer = Arc::new(Collect::default());
    let _observer = session.subscribe(observer.clone());
    session.prompt("first", &CancellationToken::new()).await.unwrap();
    session.prompt("second", &CancellationToken::new()).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(observer.events(), initial.events());
}

#[tokio::test]
async fn a_panicking_initial_sink_is_also_isolated() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("done")]));
    let runner =
        RunnerSetup::new(config(provider, vec![])).build_with(Arc::new(|_: &Event| panic!("initial sink bug")));
    let session = AgentSession::from(runner);
    let observer = Arc::new(Collect::default());
    let _observer = session.subscribe(observer.clone());
    assert_eq!(
        session.prompt("go", &CancellationToken::new()).await.unwrap().outcome,
        RunOutcome::Completed
    );
    assert!(observer.kinds().contains(&"agent_end"));
}

#[tokio::test]
async fn a_subscription_does_not_keep_the_conversation_alive() {
    let (session, _, _) = setup([]);
    let session = Arc::new(session);
    let weak = Arc::downgrade(&session);
    let _subscription = session.subscribe(Arc::new(Collect::default()));
    drop(session);
    assert!(weak.upgrade().is_none());
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
        session: SessionStore::create(&file, header()).unwrap(),
        ..RunnerSetup::new(loop_config)
    }
    .build();
    let session = AgentSession::from(runner);

    session.prompt("first", &CancellationToken::new()).await.unwrap();
    session.set_reasoning(Some(Reasoning::High)).unwrap();
    session.prompt("second", &CancellationToken::new()).await.unwrap();
    session.set_reasoning(None).unwrap();
    session.prompt("third", &CancellationToken::new()).await.unwrap();

    let sent: Vec<Option<Reasoning>> = provider.requests().iter().map(|request| request.reasoning).collect();
    assert_eq!(sent, [Some(Reasoning::Low), Some(Reasoning::High), None]);
    // Each change is a new request configuration, and every request rebuilds as sent.
    let store = SessionStore::read(&file).unwrap();
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
    assert_eq!(
        session.set_reasoning(Some(Reasoning::High)),
        Err(AgentSessionError::Busy)
    );
    session.abort();
    running.await.unwrap();
    session.prompt("second", &CancellationToken::new()).await.unwrap();
    assert_eq!(provider.faux.requests()[1].reasoning, None);
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
        Err(AgentSessionError::Busy)
    );
    session.abort();
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
            Err(AgentSessionError::InvalidModel)
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
    let session = AgentSession::from(runner);
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
            if matches!(&result, Err(AgentSessionError::Busy))
                && rejected.fetch_add(1, Ordering::SeqCst) + 1 == CALLERS - 1
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
    assert!(session.abort());
    let mut completed = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(report) => {
                assert_eq!(report.outcome, RunOutcome::Aborted);
                completed += 1;
            }
            Err(error) => assert_eq!(error, AgentSessionError::Busy),
        }
    }
    assert_eq!(completed, 1);
    assert_eq!(provider.faux.requests().len(), 1);
}

struct Ask;
#[async_trait]
impl Hooks for Ask {
    async fn before_tool(&self, _call: &zx_agent::ai::ToolCall, _ctx: &HookContext) -> HookResult<PolicyDecision> {
        Ok(PolicyDecision::Ask("review".into()))
    }
}
struct Approver(AtomicUsize);
#[async_trait]
impl Interaction for Approver {
    async fn confirm(&self, _request: &ApprovalRequest) -> Result<Approval, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Approval::AllowSession)
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
    config.hooks.push(Ask);
    config.interaction = host.clone();
    let (runner, _) = RunnerSetup::new(config).build();
    let session = AgentSession::from(runner);
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

#[tokio::test]
async fn switching_models_keeps_the_session_header_and_append_only_history() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("session.jsonl");
    let provider = Arc::new(FauxProvider::new([FauxStep::text("one")]));
    let store = SessionStore::create(&file, header()).unwrap();
    let (runner, _) = RunnerSetup {
        session: store,
        ..RunnerSetup::new(config(provider, vec![]))
    }
    .build();
    let session = AgentSession::from(runner);
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
    let reopened = SessionStore::read(&file).unwrap();
    assert_eq!(reopened.header().model, "test-model");
    assert!(
        matches!(reopened.messages().last(), Some(zx_agent::ai::Message::Assistant(message)) if message.model == "new-model")
    );
}

#[tokio::test]
async fn subscriber_callbacks_see_messages_only_after_the_single_audit_write() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("session.jsonl");
    let store = SessionStore::create(&file, header()).unwrap();
    let provider = Arc::new(FauxProvider::new([FauxStep::text("done")]));
    let (runner, _) = RunnerSetup {
        session: store,
        ..RunnerSetup::new(config(provider, vec![]))
    }
    .build();
    let session = AgentSession::from(runner);
    let observed = Arc::new(AtomicUsize::new(0));
    let mut subscriptions = Vec::new();
    for _ in 0..2 {
        let file = file.clone();
        let observed = observed.clone();
        subscriptions.push(session.subscribe(Arc::new(move |event: &Event| {
            if let AgentEvent::MessageEnd { message, .. } = &event.event {
                let stored = SessionStore::read(&file).unwrap();
                assert_eq!(stored.messages().last(), Some(message));
                observed.fetch_add(1, Ordering::SeqCst);
            }
        })));
    }
    session.prompt("go", &CancellationToken::new()).await.unwrap();
    assert_eq!(observed.load(Ordering::SeqCst), 4);
    assert_eq!(SessionStore::read(&file).unwrap().messages().len(), 2);
}

/// Refuses every record, like a database that no longer accepts writes.
struct RefusingLog;

#[async_trait]
impl zx_agent::session::SessionLog for RefusingLog {
    async fn append(&mut self, _record: &zx_agent::session::LogRecord<'_>) -> anyhow::Result<()> {
        anyhow::bail!("the database is read-only")
    }
}

#[tokio::test]
async fn a_session_reports_why_it_stopped_writing() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("never sent")]));
    let (runner, _) = RunnerSetup {
        session: SessionStore::with_log(header(), Box::new(RefusingLog)),
        ..RunnerSetup::new(config(provider, vec![]))
    }
    .build();
    let session = AgentSession::from(runner);
    let writes_stopped = || session.inspect(|store| store.writes_stopped().cloned()).unwrap();
    assert_eq!(writes_stopped(), None);

    let report = session.prompt("go", &CancellationToken::new()).await.unwrap();

    assert_eq!(report.outcome, RunOutcome::Error);
    assert!(
        matches!(
            writes_stopped(),
            Some(zx_agent::session::WritesStopped::Failed(error)) if error.contains("the database is read-only")
        ),
        "{:?}",
        writes_stopped()
    );
}
