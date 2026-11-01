//! The IM server with an in-memory channel and a scripted model: threads, access, approvals,
//! the run limit, steering, cancellation, idle release and the stop.
mod common;

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use common::{EchoTool, RunnerSetup, config, header, summarize};
use zx_agent::AgentSession;
use zx_agent::agent::{ApprovalResult, Decision, NullSink, Tool};
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::im::{
    ApprovalId, Choice, ConversationId, Destination, ImChannel, ImError, Inbound, MessageId, MessageRef, Outbound,
    Replies, Server, ServerConfig, ServerEvent, ServerSink, SessionFactory, SessionKey, ThreadId, UserId,
};
use zx_agent::interaction::Interaction;
use zx_agent::policy::{PolicyHook, Rule, RuleMatcher, RulePolicy};
use zx_agent::session::SessionStore;

const PLATFORM: &str = "fake";
const CHAT: &str = "C1";
const APPROVER: &str = "U-approver";
const OTHER: &str = "U-other";
/// Only reached when an expected message never comes.
const WAIT: Duration = Duration::from_secs(10);

/// What the server asked the channel to do.
#[derive(Debug, PartialEq)]
enum Sent {
    Post {
        to: Destination,
        message: Outbound,
        posted: MessageRef,
    },
    Update {
        posted: MessageRef,
        message: Outbound,
    },
}

/// A platform in memory: the test injects inbound events and reads what the server sent.
struct FakeChannel {
    name: &'static str,
    events: Mutex<Option<mpsc::Receiver<Inbound>>>,
    /// Passed on once cancelled, before `run` returns: deliveries acknowledged just as the
    /// server stopped.
    late: Mutex<Vec<Inbound>>,
    sent: mpsc::UnboundedSender<Sent>,
    next: AtomicUsize,
    fail: Option<String>,
}

#[async_trait]
impl ImChannel for FakeChannel {
    fn platform(&self) -> &str {
        self.name
    }

    async fn run(&self, inbound: mpsc::Sender<Inbound>, cancel: &CancellationToken) -> Result<(), ImError> {
        if let Some(error) = &self.fail {
            return Err(ImError::Config(error.clone()));
        }
        let mut events = self.events.lock().unwrap().take().expect("run is called once");
        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    let late = std::mem::take(&mut *self.late.lock().unwrap());
                    for event in late {
                        if inbound.send(event).await.is_err() {
                            break;
                        }
                    }
                    return Ok(());
                }
                Some(event) = events.recv() => {
                    if inbound.send(event).await.is_err() {
                        return Ok(());
                    }
                }
            }
        }
    }

    async fn send(&self, to: &Destination, message: &Outbound) -> Result<MessageRef, ImError> {
        let number = self.next.fetch_add(1, Ordering::SeqCst);
        let posted = MessageRef {
            at: to.clone(),
            id: MessageId(format!("out{number}")),
        };
        self.sent
            .send(Sent::Post {
                to: to.clone(),
                message: message.clone(),
                posted: posted.clone(),
            })
            .unwrap();
        Ok(posted)
    }

    async fn update(&self, posted: &MessageRef, message: &Outbound) -> Result<(), ImError> {
        self.sent
            .send(Sent::Update {
                posted: posted.clone(),
                message: message.clone(),
            })
            .unwrap();
        Ok(())
    }
}

/// Sessions on a scripted model, stored in a temporary directory. The `echo` tool asks for
/// approval; everything else is allowed.
struct Factory {
    provider: Arc<FauxProvider>,
    echo: Arc<EchoTool>,
    dir: tempfile::TempDir,
    approval_timeout: Duration,
    opened: AtomicUsize,
    fail: Option<String>,
}

#[async_trait]
impl SessionFactory for Factory {
    async fn open(&self, key: &SessionKey, interaction: Arc<dyn Interaction>) -> Result<AgentSession, String> {
        self.opened.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = &self.fail {
            return Err(error.clone());
        }
        let tools: Vec<Arc<dyn Tool>> = vec![self.echo.clone()];
        let mut config = config(self.provider.clone(), tools);
        config.interaction = interaction;
        config.interaction_timeout = self.approval_timeout;
        config.hooks.push(PolicyHook::new(Arc::new(RulePolicy::new(vec![
            Rule {
                matcher: RuleMatcher::Tool("echo".into()),
                decision: Decision::Ask("echo needs approval".into()),
            },
            Rule {
                matcher: RuleMatcher::Any,
                decision: Decision::Allow,
            },
        ]))));
        let path = key.file_in(self.dir.path());
        let store = if path.exists() {
            SessionStore::open(&path)
        } else {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            SessionStore::create(&path, header())
        }
        .map_err(|error| format!("{error:#}"))?;
        let runner = RunnerSetup {
            session: store,
            ..RunnerSetup::new(config)
        }
        .build_with(Arc::new(NullSink));
        Ok(AgentSession::from(runner))
    }
}

struct Events(mpsc::UnboundedSender<ServerEvent>);

impl ServerSink for Events {
    fn emit(&self, event: &ServerEvent) {
        // The test may have stopped reading.
        self.0.send(event.clone()).ok();
    }
}

struct Setup {
    steps: Vec<FauxStep>,
    config: ServerConfig,
    approval_timeout: Duration,
    fail_open: Option<String>,
    fail_channel: Option<String>,
    /// Events the channel passes on as the server stops.
    late: Vec<Inbound>,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            steps: Vec::new(),
            config: ServerConfig {
                conversations: vec![(PLATFORM.into(), ConversationId(CHAT.into()))],
                approvers: vec![(PLATFORM.into(), UserId(APPROVER.into()))],
                ..ServerConfig::default()
            },
            approval_timeout: WAIT,
            fail_open: None,
            fail_channel: None,
            late: Vec::new(),
        }
    }
}

struct Harness {
    inject: mpsc::Sender<Inbound>,
    sent: mpsc::UnboundedReceiver<Sent>,
    events: mpsc::UnboundedReceiver<ServerEvent>,
    cancel: CancellationToken,
    server: JoinHandle<Result<(), ImError>>,
    factory: Arc<Factory>,
    replies: Replies,
}

fn start(setup: Setup) -> Harness {
    let factory = Arc::new(Factory {
        provider: Arc::new(FauxProvider::new(setup.steps)),
        echo: Arc::new(EchoTool::new("echo")),
        dir: tempfile::tempdir().unwrap(),
        approval_timeout: setup.approval_timeout,
        opened: AtomicUsize::new(0),
        fail: setup.fail_open,
    });
    let (inject, events_in) = mpsc::channel(16);
    let (sent_to, sent) = mpsc::unbounded_channel();
    let channel: Arc<dyn ImChannel> = Arc::new(FakeChannel {
        name: PLATFORM,
        events: Mutex::new(Some(events_in)),
        late: Mutex::new(setup.late),
        sent: sent_to,
        next: AtomicUsize::new(0),
        fail: setup.fail_channel,
    });
    let (events_to, events) = mpsc::unbounded_channel();
    let replies = setup.config.replies.clone();
    let server = Server::new(setup.config, factory.clone(), Arc::new(Events(events_to)));
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    let server = tokio::spawn(async move { server.run(vec![channel], &stop).await });
    Harness {
        inject,
        sent,
        events,
        cancel,
        server,
        factory,
        replies,
    }
}

fn thread(root: &str) -> Destination {
    Destination {
        conversation: ConversationId(CHAT.into()),
        thread: Some(ThreadId(root.into())),
    }
}

fn text(text: &str) -> Outbound {
    Outbound::Text(text.into())
}

/// A message in `chat`; `thread` is `None` for one posted in the chat itself.
fn message(chat: &str, id: &str, thread: Option<&str>, text: &str) -> Inbound {
    Inbound::Message {
        id: MessageId(id.into()),
        at: Destination {
            conversation: ConversationId(chat.into()),
            thread: thread.map(|root| ThreadId(root.into())),
        },
        sender: UserId("U-asker".into()),
        text: text.into(),
    }
}

impl Harness {
    async fn say_in(&self, chat: &str, id: &str, thread: Option<&str>, text: &str) {
        self.inject.send(message(chat, id, thread, text)).await.unwrap();
    }

    async fn say(&self, id: &str, thread: Option<&str>, text: &str) {
        self.say_in(CHAT, id, thread, text).await;
    }

    async fn click(&self, root: &str, user: &str, approval: &ApprovalId, choice: Choice) {
        let click = Inbound::Decision {
            at: thread(root),
            user: UserId(user.into()),
            approval: approval.clone(),
            choice,
        };
        self.inject.send(click).await.unwrap();
    }

    async fn next(&mut self) -> Sent {
        tokio::time::timeout(WAIT, self.sent.recv())
            .await
            .expect("the server sent nothing in time")
            .expect("the channel is alive")
    }

    /// Expect `message` posted to `to`, and return where it was posted.
    async fn posted(&mut self, to: &Destination, message: Outbound) -> MessageRef {
        match self.next().await {
            Sent::Post {
                to: actual,
                message: sent,
                posted,
            } => {
                assert_eq!((&actual, &sent), (to, &message));
                posted
            }
            other => panic!("expected a post of {message:?}, got {other:?}"),
        }
    }

    async fn updated(&mut self, posted: &MessageRef, message: Outbound) {
        match self.next().await {
            Sent::Update {
                posted: actual,
                message: sent,
            } => assert_eq!((&actual, &sent), (posted, &message)),
            other => panic!("expected an update to {message:?}, got {other:?}"),
        }
    }

    /// Expect an approval card for the echo call `{"text": "done"}` in `to`.
    async fn card(&mut self, to: &Destination) -> (ApprovalId, MessageRef) {
        match self.next().await {
            Sent::Post {
                to: actual,
                message:
                    Outbound::ApprovalRequest {
                        id,
                        tool,
                        summary,
                        reason,
                    },
                posted,
            } => {
                assert_eq!(&actual, to);
                assert_eq!(
                    (tool.as_str(), summary.as_str(), reason.as_str()),
                    ("echo", r#"{"text":"done"}"#, "echo needs approval")
                );
                (id, posted)
            }
            other => panic!("expected an approval card, got {other:?}"),
        }
    }

    async fn event(&mut self) -> ServerEvent {
        tokio::time::timeout(WAIT, self.events.recv())
            .await
            .expect("the server reported nothing in time")
            .expect("the sink is alive")
    }

    fn requests(&self) -> Vec<Vec<String>> {
        self.factory
            .provider
            .requests()
            .iter()
            .map(|request| summarize(&request.messages))
            .collect()
    }

    async fn stop(self) -> Result<(), ImError> {
        self.cancel.cancel();
        self.server.await.unwrap()
    }

    /// Stop the server and wait until it has returned; then take what it sent and reported
    /// meanwhile. Conversations stop concurrently, so different threads may interleave.
    async fn stop_and_take(mut self) -> (Vec<Sent>, Vec<ServerEvent>) {
        self.cancel.cancel();
        (&mut self.server).await.unwrap().unwrap();
        let sent = std::iter::from_fn(|| self.sent.try_recv().ok()).collect();
        let events = std::iter::from_fn(|| self.events.try_recv().ok()).collect();
        (sent, events)
    }
}

fn key(root: &str) -> SessionKey {
    SessionKey {
        platform: PLATFORM.into(),
        conversation: ConversationId(CHAT.into()),
        thread: ThreadId(root.into()),
    }
}

/// The operator's record of a thread's messages that will not run.
fn unanswered(root: &str, count: usize) -> ServerEvent {
    ServerEvent::Failed {
        key: Some(key(root)),
        error: format!("{count} message(s) were not answered: the server is stopping"),
    }
}

fn echo_call() -> FauxStep {
    FauxStep::tool_call("c1", "echo", json!({"text": "done"}))
}

#[tokio::test]
async fn a_message_in_the_chat_is_answered_in_a_thread_under_it() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("answer")],
        ..Setup::default()
    });
    h.say("m1", None, "hello").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working.clone())).await;
    h.updated(&status, text("answer")).await;
    assert_eq!(h.requests(), [["user:hello"]]);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_reply_in_the_thread_continues_its_session() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one"), FauxStep::text("two")],
        ..Setup::default()
    });
    let working = text(&h.replies.working.clone());
    h.say("m1", None, "first").await;
    let status = h.posted(&thread("m1"), working.clone()).await;
    h.updated(&status, text("one")).await;
    h.say("m2", Some("m1"), "second").await;
    let status = h.posted(&thread("m1"), working).await;
    h.updated(&status, text("two")).await;
    assert_eq!(h.requests()[1], ["user:first", "assistant:one", "user:second"]);
    assert_eq!(h.factory.opened.load(Ordering::SeqCst), 1);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn each_thread_has_a_session_of_its_own() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one"), FauxStep::text("two")],
        ..Setup::default()
    });
    let working = text(&h.replies.working.clone());
    h.say("m1", None, "first").await;
    let status = h.posted(&thread("m1"), working.clone()).await;
    h.updated(&status, text("one")).await;
    h.say("m2", None, "unrelated").await;
    let status = h.posted(&thread("m2"), working).await;
    h.updated(&status, text("two")).await;
    assert_eq!(h.requests()[1], ["user:unrelated"]);
    assert_eq!(h.factory.opened.load(Ordering::SeqCst), 2);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_chat_that_is_not_served_gets_a_refusal_and_nothing_else() {
    let mut h = start(Setup::default());
    h.say_in("C2", "m1", None, "hello").await;
    let refused = Destination {
        conversation: ConversationId("C2".into()),
        thread: Some(ThreadId("m1".into())),
    };
    h.posted(&refused, text(&h.replies.unauthorized.clone())).await;
    assert_eq!(h.factory.opened.load(Ordering::SeqCst), 0);
    assert!(h.requests().is_empty());
    h.stop().await.unwrap();
}

#[tokio::test]
async fn an_approver_allows_a_call_and_the_card_shows_the_decision() {
    let mut h = start(Setup {
        steps: vec![echo_call(), FauxStep::text("finished")],
        ..Setup::default()
    });
    h.say("m1", None, "run echo").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working.clone())).await;
    let (id, card) = h.card(&thread("m1")).await;
    h.click("m1", APPROVER, &id, Choice::AllowOnce).await;
    h.updated(
        &card,
        Outbound::ApprovalResolved {
            id: id.clone(),
            user: Some(UserId(APPROVER.into())),
            result: ApprovalResult::AllowOnce,
        },
    )
    .await;
    h.updated(&status, text("finished")).await;
    assert_eq!(h.factory.echo.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        h.requests()[1],
        ["user:run echo", "assistant:calls[c1]", "tool:c1=done"]
    );
    match h.event().await {
        ServerEvent::Approval(record) => {
            assert_eq!(
                (
                    record.tool_call_id.as_str(),
                    record.tool.as_str(),
                    record.approval,
                    record.user,
                    record.result
                ),
                (
                    "c1",
                    "echo",
                    Some(id),
                    Some(UserId(APPROVER.into())),
                    ApprovalResult::AllowOnce
                )
            );
            assert!(record.ended_ms > 0);
        }
        other => panic!("expected the approval record, got {other:?}"),
    }
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_click_by_someone_who_is_not_an_approver_decides_nothing() {
    let mut h = start(Setup {
        steps: vec![echo_call(), FauxStep::text("not run")],
        ..Setup::default()
    });
    h.say("m1", None, "run echo").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working.clone())).await;
    let (id, card) = h.card(&thread("m1")).await;
    h.click("m1", OTHER, &id, Choice::AllowOnce).await;
    h.posted(&thread("m1"), text(&h.replies.not_approver.clone())).await;
    h.click("m1", APPROVER, &id, Choice::Deny).await;
    h.updated(
        &card,
        Outbound::ApprovalResolved {
            id,
            user: Some(UserId(APPROVER.into())),
            result: ApprovalResult::Denied,
        },
    )
    .await;
    h.updated(&status, text("not run")).await;
    assert_eq!(h.factory.echo.calls.load(Ordering::SeqCst), 0);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn an_unanswered_approval_times_out_and_its_card_says_so() {
    let mut h = start(Setup {
        steps: vec![echo_call(), FauxStep::text("gave up")],
        approval_timeout: Duration::from_millis(50),
        ..Setup::default()
    });
    h.say("m1", None, "run echo").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working.clone())).await;
    let (id, card) = h.card(&thread("m1")).await;
    h.updated(
        &card,
        Outbound::ApprovalResolved {
            id,
            user: None,
            result: ApprovalResult::TimedOut,
        },
    )
    .await;
    h.updated(&status, text("gave up")).await;
    assert_eq!(h.factory.echo.calls.load(Ordering::SeqCst), 0);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn cancel_in_the_thread_stops_the_run_and_closes_its_card() {
    let mut h = start(Setup {
        steps: vec![echo_call(), FauxStep::text("unused")],
        ..Setup::default()
    });
    h.say("m1", None, "run echo").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working.clone())).await;
    let (id, card) = h.card(&thread("m1")).await;
    h.say("m2", Some("m1"), " /cancel ").await;
    h.updated(
        &card,
        Outbound::ApprovalResolved {
            id,
            user: None,
            result: ApprovalResult::Cancelled,
        },
    )
    .await;
    h.updated(&status, text(&h.replies.cancelled.clone())).await;
    assert_eq!(h.factory.echo.calls.load(Ordering::SeqCst), 0);
    assert_eq!(h.requests().len(), 1);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn cancel_with_nothing_running_says_so() {
    let mut h = start(Setup::default());
    h.say("m1", None, "/cancel").await;
    h.posted(&thread("m1"), text(&h.replies.nothing_to_cancel.clone()))
        .await;
    assert_eq!(h.factory.opened.load(Ordering::SeqCst), 0);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn the_run_limit_makes_other_threads_wait_their_turn() {
    let mut h = start(Setup {
        steps: vec![echo_call(), FauxStep::text("a done"), FauxStep::text("b done")],
        config: ServerConfig {
            max_running: NonZeroUsize::MIN,
            ..Setup::default().config
        },
        ..Setup::default()
    });
    let working = text(&h.replies.working.clone());
    h.say("a", None, "task a").await;
    let status_a = h.posted(&thread("a"), working.clone()).await;
    // Thread a now waits for an approval while holding the only run slot.
    let (id, card) = h.card(&thread("a")).await;
    h.say("b", None, "task b").await;
    h.posted(&thread("b"), text(&h.replies.queued.clone())).await;
    h.click("a", APPROVER, &id, Choice::AllowOnce).await;
    h.updated(
        &card,
        Outbound::ApprovalResolved {
            id,
            user: Some(UserId(APPROVER.into())),
            result: ApprovalResult::AllowOnce,
        },
    )
    .await;
    h.updated(&status_a, text("a done")).await;
    let status_b = h.posted(&thread("b"), working).await;
    h.updated(&status_b, text("b done")).await;
    assert_eq!(h.requests()[2], ["user:task b"]);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_message_during_a_run_is_steered_into_it() {
    let mut h = start(Setup {
        steps: vec![echo_call(), FauxStep::text("final")],
        ..Setup::default()
    });
    h.say("m1", None, "run echo").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working.clone())).await;
    let (id, card) = h.card(&thread("m1")).await;
    h.say("m2", Some("m1"), "also check the logs").await;
    h.click("m1", APPROVER, &id, Choice::AllowOnce).await;
    h.updated(
        &card,
        Outbound::ApprovalResolved {
            id,
            user: Some(UserId(APPROVER.into())),
            result: ApprovalResult::AllowOnce,
        },
    )
    .await;
    h.updated(&status, text("final")).await;
    assert_eq!(
        h.requests()[1],
        [
            "user:run echo",
            "assistant:calls[c1]",
            "tool:c1=done",
            "user:also check the logs"
        ]
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn an_idle_conversation_releases_its_session_and_reopens_it() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one"), FauxStep::text("two")],
        config: ServerConfig {
            idle: Duration::from_millis(20),
            ..Setup::default().config
        },
        ..Setup::default()
    });
    let working = text(&h.replies.working.clone());
    h.say("m1", None, "first").await;
    let status = h.posted(&thread("m1"), working.clone()).await;
    h.updated(&status, text("one")).await;
    let key = SessionKey {
        platform: PLATFORM.into(),
        conversation: ConversationId(CHAT.into()),
        thread: ThreadId("m1".into()),
    };
    assert_eq!(h.event().await, ServerEvent::Released { key });
    h.say("m2", Some("m1"), "second").await;
    let status = h.posted(&thread("m1"), working).await;
    h.updated(&status, text("two")).await;
    assert_eq!(h.factory.opened.load(Ordering::SeqCst), 2);
    assert_eq!(h.requests()[1], ["user:first", "assistant:one", "user:second"]);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn stopping_the_server_cancels_runs_and_posts_how_they_ended() {
    let mut h = start(Setup {
        steps: vec![FauxStep::Hang],
        ..Setup::default()
    });
    h.say("m1", None, "wait forever").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working.clone())).await;
    h.cancel.cancel();
    h.updated(&status, text(&h.replies.cancelled.clone())).await;
    h.server.await.unwrap().unwrap();
}

#[tokio::test]
async fn stopping_tells_a_thread_waiting_for_a_run_slot_that_it_will_not_run() {
    let mut h = start(Setup {
        steps: vec![FauxStep::Hang],
        config: ServerConfig {
            max_running: NonZeroUsize::MIN,
            ..Setup::default().config
        },
        ..Setup::default()
    });
    let replies = h.replies.clone();
    h.say("a", None, "task a").await;
    let status_a = h.posted(&thread("a"), text(&replies.working)).await;
    h.say("b", None, "task b").await;
    h.posted(&thread("b"), text(&replies.queued)).await;
    let (sent, events) = h.stop_and_take().await;
    let (mut in_a, mut in_b) = (Vec::new(), Vec::new());
    for sent in sent {
        match sent {
            Sent::Update { posted, message } if posted == status_a => in_a.push(message),
            Sent::Post { to, message, .. } if to == thread("b") => in_b.push(message),
            other => panic!("unexpected {other:?}"),
        }
    }
    // The run had taken in all its input, so its thread only learns how it ended. Thread b
    // never ran: a run posts its status first.
    assert_eq!(in_a, [text(&replies.cancelled)]);
    assert_eq!(in_b, [text(&replies.stopping)]);
    assert_eq!(events, [unanswered("b", 1)]);
}

#[tokio::test]
async fn stopping_tells_a_thread_about_steering_its_run_did_not_take_in() {
    let mut h = start(Setup {
        steps: vec![echo_call()],
        ..Setup::default()
    });
    let replies = h.replies.clone();
    h.say("m1", None, "run echo").await;
    let status = h.posted(&thread("m1"), text(&replies.working)).await;
    let (id, card) = h.card(&thread("m1")).await;
    h.say("m2", Some("m1"), "also check the logs").await;
    // The worker answers this click only after it has steered the message before it.
    h.click("m1", OTHER, &id, Choice::AllowOnce).await;
    h.posted(&thread("m1"), text(&replies.not_approver)).await;
    let (sent, events) = h.stop_and_take().await;
    let closed = Outbound::ApprovalResolved {
        id,
        user: None,
        result: ApprovalResult::Cancelled,
    };
    match sent.as_slice() {
        [first, second, Sent::Post { to, message, .. }] => {
            assert_eq!(
                first,
                &Sent::Update {
                    posted: card,
                    message: closed
                }
            );
            assert_eq!(
                second,
                &Sent::Update {
                    posted: status,
                    message: text(&replies.cancelled)
                }
            );
            assert_eq!((to, message), (&thread("m1"), &text(&replies.stopping)));
        }
        other => panic!("expected the card, the status and a notice, got {other:?}"),
    }
    assert!(
        matches!(events.as_slice(), [ServerEvent::Approval(_), last] if *last == unanswered("m1", 1)),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_message_received_as_the_server_stops_is_answered_without_running() {
    let h = start(Setup {
        late: vec![message(CHAT, "m1", None, "hello")],
        ..Setup::default()
    });
    let replies = h.replies.clone();
    let factory = Arc::clone(&h.factory);
    let (sent, events) = h.stop_and_take().await;
    match sent.as_slice() {
        [Sent::Post { to, message, .. }] => assert_eq!((to, message), (&thread("m1"), &text(&replies.stopping))),
        other => panic!("expected one notice, got {other:?}"),
    }
    assert_eq!(events, [unanswered("m1", 1)]);
    assert_eq!(
        factory.opened.load(Ordering::SeqCst),
        0,
        "no session is opened to refuse"
    );
}

#[tokio::test]
async fn a_failing_channel_stops_the_server_with_its_error() {
    let h = start(Setup {
        fail_channel: Some("token rejected".into()),
        ..Setup::default()
    });
    match h.server.await.unwrap() {
        Err(ImError::Config(error)) => assert_eq!(error, "token rejected"),
        other => panic!("expected the channel's error, got {other:?}"),
    }
}

#[tokio::test]
async fn a_session_that_cannot_be_opened_is_reported_and_tried_again() {
    let mut h = start(Setup {
        fail_open: Some("disk full".into()),
        ..Setup::default()
    });
    let refused = text(&format!("{}disk full", h.replies.cannot_open));
    h.say("m1", None, "hello").await;
    h.posted(&thread("m1"), refused.clone()).await;
    match h.event().await {
        ServerEvent::Failed { key: Some(key), error } => {
            assert_eq!(key.thread, ThreadId("m1".into()));
            assert!(error.contains("disk full"), "{error}");
        }
        other => panic!("expected a failure, got {other:?}"),
    }
    h.say("m2", Some("m1"), "again").await;
    h.posted(&thread("m1"), refused).await;
    assert_eq!(h.factory.opened.load(Ordering::SeqCst), 2);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn two_channels_with_one_name_are_a_configuration_error() {
    let channel = || -> Arc<dyn ImChannel> {
        let (sent, _) = mpsc::unbounded_channel();
        Arc::new(FakeChannel {
            name: PLATFORM,
            events: Mutex::new(None),
            late: Mutex::new(Vec::new()),
            sent,
            next: AtomicUsize::new(0),
            fail: None,
        })
    };
    let h = start(Setup::default());
    let server = Server::new(ServerConfig::default(), h.factory.clone(), Arc::new(NullServerSink));
    let result = server.run(vec![channel(), channel()], &CancellationToken::new()).await;
    assert!(matches!(result, Err(ImError::Config(error)) if error.contains("fake")));
    h.stop().await.unwrap();
}

struct NullServerSink;

impl ServerSink for NullServerSink {
    fn emit(&self, _event: &ServerEvent) {}
}
