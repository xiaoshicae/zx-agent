//! The IM server with an in-memory channel and a scripted model: threads, access, approvals,
//! the run limit, steering, cancellation, idle release, the stop, and the statuses kept for a
//! restart.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use zx_agent::Agent;
use zx_agent::agent::{SessionParts, SessionSetup};
use zx_agent::ai::Message;
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::builtin::policy::{Rule, RuleMatcher, RulePolicy};
use zx_agent::events::ApprovalResult;
use zx_agent::policy::{ApprovalMode, PolicyDecision};
use zx_agent::tools::Tool;
use zx_agent::transcript::{
    Entry, LoadedSession, MemoryStorage, SessionHeader, SessionList, SessionLog, SessionQuery, SessionStorage,
    Transcript,
};
use zx_im::{
    Addressee, ApprovalCard, ApprovalId, Backlog, ChatKind, Choice, ConversationId, Destination, ImChannel, ImError,
    Inbound, Inlet, Kept, MemoryBacklog, MessageId, MessageRef, Post, Replacement, Replies, Said, Server, ServerConfig,
    ServerEvent, ServerSink, SessionKey, StatusStore, ThreadId, UserId,
};
use zx_testing::{EchoTool, config, fast_retry, summarize, test_limits};

const PLATFORM: &str = "fake";
const CHAT: &str = "C1";
/// A direct chat with the asker.
const DIRECT: &str = "D1";
const ASKER: &str = "U-asker";
const APPROVER: &str = "U-approver";
const OTHER: &str = "U-other";
/// Only reached when an expected message never comes.
const WAIT: Duration = Duration::from_secs(10);

/// What the server asked the channel to do.
#[derive(Debug, PartialEq)]
enum Sent {
    Post {
        to: Destination,
        message: Post,
        posted: MessageRef,
    },
    Update {
        posted: MessageRef,
        message: Replacement,
    },
    /// A status kept in [`Statuses`].
    Kept(MessageRef),
    /// A status forgotten by [`Statuses`].
    Forgot(MessageRef),
    /// A message acknowledged as taken in.
    Acknowledged(MessageRef),
}

/// How the platform answers an update; it records the update either way.
#[derive(Clone, Default)]
enum Updates {
    #[default]
    Done,
    Refused,
    /// Never answers.
    Hang,
    /// Answers once the test cancels the token, and at once from then on.
    Held(CancellationToken),
}

/// Deliver `event` as a platform's channel does: kept first, then passed on unless the
/// backlog has it already. False once the server no longer reads.
async fn deliver(inlet: &Inlet, event: Inbound) -> bool {
    match inlet.keep(&event).await.unwrap() {
        Kept::New => inlet.pass(event).await,
        Kept::Again => true,
    }
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
    updates: Updates,
    /// What every thread holds before the bot is mentioned there.
    history: Vec<Said>,
    /// Holds each reading of a thread's history, when there is one.
    history_gate: Option<Arc<Gate>>,
    /// Users' names; the others have none.
    names: HashMap<String, String>,
}

#[async_trait]
impl ImChannel for FakeChannel {
    async fn thread_history(&self, _at: &Destination, _limit: usize) -> Result<Vec<Said>, ImError> {
        if let Some(gate) = &self.history_gate {
            gate.pass().await;
        }
        Ok(self.history.clone())
    }

    async fn display_name(&self, user: &UserId) -> Result<Option<String>, ImError> {
        Ok(self.names.get(&user.0).cloned())
    }

    async fn acknowledge(&self, message: &MessageRef) -> Result<(), ImError> {
        self.sent.send(Sent::Acknowledged(message.clone())).unwrap();
        Ok(())
    }

    fn platform(&self) -> &str {
        self.name
    }

    async fn run(&self, inlet: Inlet, cancel: &CancellationToken) -> Result<(), ImError> {
        if let Some(error) = &self.fail {
            return Err(ImError::Config(error.clone()));
        }
        let mut events = self.events.lock().unwrap().take().expect("run is called once");
        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    let late = std::mem::take(&mut *self.late.lock().unwrap());
                    for event in late {
                        if !deliver(&inlet, event).await {
                            break;
                        }
                    }
                    return Ok(());
                }
                Some(event) = events.recv() => {
                    if !deliver(&inlet, event).await {
                        return Ok(());
                    }
                }
            }
        }
    }

    async fn send(&self, to: &Destination, message: &Post) -> Result<MessageRef, ImError> {
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

    async fn update(&self, posted: &MessageRef, message: &Replacement) -> Result<(), ImError> {
        self.sent
            .send(Sent::Update {
                posted: posted.clone(),
                message: message.clone(),
            })
            .unwrap();
        match &self.updates {
            Updates::Done => Ok(()),
            Updates::Refused => Err(ImError::Transport("update refused".into())),
            Updates::Hang => std::future::pending().await,
            Updates::Held(release) => {
                release.cancelled().await;
                Ok(())
            }
        }
    }
}

/// A call of [`Statuses`] that fails with [`STORE_ERROR`].
#[derive(Clone, Copy, PartialEq)]
enum Broken {
    Insert,
    Remove,
    List,
}

const STORE_ERROR: &str = "store unavailable";

/// Statuses in memory. Each insertion and removal, failed or not, also goes to the test among
/// what the channel was asked to do, so that a test sees their order.
struct Statuses {
    kept: Mutex<Vec<(SessionKey, MessageRef)>>,
    sent: mpsc::UnboundedSender<Sent>,
    broken: Option<Broken>,
}

impl Statuses {
    fn kept(&self) -> Vec<(SessionKey, MessageRef)> {
        self.kept.lock().unwrap().clone()
    }
}

#[async_trait]
impl StatusStore for Statuses {
    async fn insert(&self, key: &SessionKey, status: &MessageRef) -> Result<(), String> {
        if self.broken != Some(Broken::Insert) {
            self.kept.lock().unwrap().push((key.clone(), status.clone()));
        }
        self.sent.send(Sent::Kept(status.clone())).unwrap();
        match self.broken {
            Some(Broken::Insert) => Err(STORE_ERROR.into()),
            _ => Ok(()),
        }
    }

    async fn remove(&self, _key: &SessionKey, status: &MessageRef) -> Result<(), String> {
        if self.broken != Some(Broken::Remove) {
            self.kept.lock().unwrap().retain(|(_, kept)| kept != status);
        }
        self.sent.send(Sent::Forgot(status.clone())).unwrap();
        match self.broken {
            Some(Broken::Remove) => Err(STORE_ERROR.into()),
            _ => Ok(()),
        }
    }

    async fn list(&self) -> Result<Vec<(SessionKey, MessageRef)>, String> {
        match self.broken {
            Some(Broken::List) => Err(STORE_ERROR.into()),
            _ => Ok(self.kept()),
        }
    }
}

/// Holds what reaches it until it is released, once armed: the setup of the sessions of the
/// thread `slow-thread`, or the reading of a thread's history.
struct Gate {
    armed: AtomicBool,
    entered: tokio::sync::Notify,
    release: CancellationToken,
}

impl Gate {
    fn new() -> Arc<Self> {
        let gate = Self::disarmed();
        gate.arm();
        gate
    }

    /// A gate that lets everything pass until it is armed.
    fn disarmed() -> Arc<Self> {
        Arc::new(Self {
            armed: AtomicBool::new(false),
            entered: tokio::sync::Notify::new(),
            release: CancellationToken::new(),
        })
    }

    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    /// Wait here until released, if armed.
    async fn pass(&self) {
        if self.armed.load(Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.cancelled().await;
        }
    }
}

/// What the tests know of the sessions the server opens: the scripted model, the `echo`
/// tool, the storage that keeps them in memory, how often a session was set up, and how
/// often the server read the storage's headers or listed sessions. It is the agent's session
/// setup, which refuses every session when it has a reason to and can be held at a gate, and
/// its storage, which counts what it is asked and no longer has the sessions it was told to
/// forget, as when their files are removed.
struct Sessions {
    provider: Arc<FauxProvider>,
    echo: Arc<EchoTool>,
    storage: Arc<MemoryStorage>,
    opened: AtomicUsize,
    indexed: AtomicUsize,
    listed: AtomicUsize,
    fail: Option<String>,
    setup_gate: Option<Arc<Gate>>,
    forgotten: Mutex<HashSet<String>>,
}

impl Sessions {
    /// Lose the session `id` from the storage.
    fn forget(&self, id: &str) {
        self.forgotten.lock().unwrap().insert(id.to_string());
    }

    fn is_forgotten(&self, id: &str) -> bool {
        self.forgotten.lock().unwrap().contains(id)
    }

    /// The IDs of the conversation's sessions, most recently active first.
    async fn of(&self, key: &SessionKey) -> Result<Vec<String>, String> {
        let query = SessionQuery {
            labels: key.labels(),
            ..SessionQuery::default()
        };
        let list = self.storage.list(&query).await.map_err(|error| format!("{error:#}"))?;
        Ok(list.sessions.into_iter().map(|session| session.id).collect())
    }

    /// The agent of every conversation: its `echo` tool asks for approval, which waits
    /// `approval_timeout`; everything else is allowed.
    fn agent(self: &Arc<Self>, approval_timeout: Duration) -> Agent {
        let tools: Vec<Arc<dyn Tool>> = vec![self.echo.clone()];
        let mut config = config(self.provider.clone(), tools);
        config.approvals = ApprovalMode::Wait(approval_timeout);
        config.policies.push(
            "echo_approval",
            Arc::new(RulePolicy::new(vec![
                Rule {
                    matcher: RuleMatcher::Tool("echo".into()),
                    decision: PolicyDecision::Ask("echo needs approval".into()),
                },
                Rule {
                    matcher: RuleMatcher::Any,
                    decision: PolicyDecision::Allow,
                },
            ])),
        );
        config.retry = fast_retry(0);
        config.limits = test_limits();
        config.storage = self.clone();
        config.session_setup = Some(self.clone());
        Agent::new(config).unwrap()
    }
}

#[async_trait]
impl SessionSetup for Sessions {
    async fn setup(&self, header: &SessionHeader) -> anyhow::Result<SessionParts> {
        self.opened.fetch_add(1, Ordering::SeqCst);
        if header.labels.values().any(|value| value == "slow-thread")
            && let Some(gate) = &self.setup_gate
        {
            gate.pass().await;
        }
        match &self.fail {
            Some(reason) => Err(anyhow::anyhow!("{reason}")),
            None => Ok(SessionParts::default()),
        }
    }
}

#[async_trait]
impl SessionStorage for Sessions {
    async fn create(&self, header: &SessionHeader) -> anyhow::Result<Box<dyn SessionLog>> {
        self.storage.create(header).await
    }

    async fn open(&self, id: &str) -> anyhow::Result<(LoadedSession, Box<dyn SessionLog>)> {
        anyhow::ensure!(!self.is_forgotten(id), "no session {id}");
        self.storage.open(id).await
    }

    async fn read(&self, id: &str) -> anyhow::Result<LoadedSession> {
        self.storage.read(id).await
    }

    async fn headers(&self, query: &SessionQuery) -> anyhow::Result<Vec<SessionHeader>> {
        self.indexed.fetch_add(1, Ordering::SeqCst);
        let mut headers = self.storage.headers(query).await?;
        headers.retain(|header| !self.is_forgotten(&header.id));
        Ok(headers)
    }

    async fn list(&self, query: &SessionQuery) -> anyhow::Result<SessionList> {
        self.listed.fetch_add(1, Ordering::SeqCst);
        self.storage.list(query).await
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
    setup_gate: Option<Arc<Gate>>,
    history_gate: Option<Arc<Gate>>,
    /// Where the sessions are kept: a new storage without one, the one an earlier server
    /// left with one.
    storage: Option<Arc<MemoryStorage>>,
    /// Where the messages are kept until handled, the one an earlier server left included.
    backlog: Arc<MemoryBacklog>,
    fail_channel: Option<String>,
    /// Events the channel passes on as the server stops.
    late: Vec<Inbound>,
    /// Statuses an earlier process left behind; `None` keeps no statuses.
    statuses: Option<Vec<(SessionKey, MessageRef)>>,
    broken: Option<Broken>,
    updates: Updates,
    history: Vec<Said>,
    names: HashMap<String, String>,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            steps: Vec::new(),
            config: ServerConfig {
                conversations: vec![
                    (PLATFORM.into(), ConversationId(CHAT.into())),
                    (PLATFORM.into(), ConversationId(DIRECT.into())),
                ],
                approvers: vec![(PLATFORM.into(), UserId(APPROVER.into()))],
                ..ServerConfig::default()
            },
            approval_timeout: WAIT,
            fail_open: None,
            setup_gate: None,
            history_gate: None,
            storage: None,
            backlog: Arc::default(),
            fail_channel: None,
            late: Vec::new(),
            statuses: None,
            broken: None,
            updates: Updates::default(),
            history: Vec::new(),
            names: HashMap::new(),
        }
    }
}

struct Harness {
    inject: mpsc::Sender<Inbound>,
    sent: mpsc::UnboundedReceiver<Sent>,
    events: mpsc::UnboundedReceiver<ServerEvent>,
    cancel: CancellationToken,
    server: JoinHandle<Result<(), ImError>>,
    sessions: Arc<Sessions>,
    agent: Agent,
    replies: Replies,
    statuses: Option<Arc<Statuses>>,
    backlog: Arc<MemoryBacklog>,
}

fn start(setup: Setup) -> Harness {
    let sessions = Arc::new(Sessions {
        provider: Arc::new(FauxProvider::new(setup.steps)),
        echo: Arc::new(EchoTool::new("echo")),
        storage: setup.storage.unwrap_or_default(),
        opened: AtomicUsize::new(0),
        indexed: AtomicUsize::new(0),
        listed: AtomicUsize::new(0),
        fail: setup.fail_open,
        setup_gate: setup.setup_gate,
        forgotten: Mutex::default(),
    });
    let agent = sessions.agent(setup.approval_timeout);
    let (inject, events_in) = mpsc::channel(16);
    let (sent_to, sent) = mpsc::unbounded_channel();
    let statuses = setup.statuses.map(|left| {
        Arc::new(Statuses {
            kept: Mutex::new(left),
            sent: sent_to.clone(),
            broken: setup.broken,
        })
    });
    let channel: Arc<dyn ImChannel> = Arc::new(FakeChannel {
        name: PLATFORM,
        events: Mutex::new(Some(events_in)),
        late: Mutex::new(setup.late),
        sent: sent_to,
        next: AtomicUsize::new(0),
        fail: setup.fail_channel,
        updates: setup.updates,
        history: setup.history,
        history_gate: setup.history_gate,
        names: setup.names,
    });
    let (events_to, events) = mpsc::unbounded_channel();
    let mut config = setup.config;
    if let Some(statuses) = &statuses {
        let store: Arc<dyn StatusStore> = statuses.clone();
        config.statuses = Some(store);
    }
    let backlog = setup.backlog;
    config.backlog = backlog.clone();
    let replies = config.replies.clone();
    let server = Server::new(config, agent.clone(), Arc::new(Events(events_to)));
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    let server = tokio::spawn(async move { server.run(vec![channel], &stop).await });
    Harness {
        inject,
        sent,
        events,
        cancel,
        server,
        sessions,
        agent,
        replies,
        statuses,
        backlog,
    }
}

fn thread(root: &str) -> Destination {
    Destination {
        conversation: ConversationId(CHAT.into()),
        thread: Some(ThreadId(root.into())),
    }
}

/// What the server sends that can be plain text: a post, or what replaces one.
trait Plain {
    fn plain(text: String) -> Self;
}

impl Plain for Post {
    fn plain(text: String) -> Self {
        Self::Text(text)
    }
}

impl Plain for Replacement {
    fn plain(text: String) -> Self {
        Self::Text(text)
    }
}

/// Plain text, as a post or as what replaces one: whichever the place it is used in takes.
fn text<T: Plain>(text: &str) -> T {
    T::plain(text.into())
}

/// The direct chat with the asker.
fn direct() -> Destination {
    Destination {
        conversation: ConversationId(DIRECT.into()),
        thread: None,
    }
}

/// A message from the asker in group `chat` that mentions the bot; `thread` is `None` for one
/// posted in the chat itself.
fn message(chat: &str, id: &str, thread: Option<&str>, text: &str) -> Inbound {
    group_message(chat, id, thread, ASKER, Addressee::Bot, text)
}

fn group_message(
    chat: &str,
    id: &str,
    thread: Option<&str>,
    sender: &str,
    addressee: Addressee,
    text: &str,
) -> Inbound {
    Inbound::Message {
        id: MessageId(id.into()),
        at: Destination {
            conversation: ConversationId(chat.into()),
            thread: thread.map(|root| ThreadId(root.into())),
        },
        chat: ChatKind::Group,
        addressee,
        sender: UserId(sender.into()),
        text: text.into(),
    }
}

impl Harness {
    async fn say_in(&self, chat: &str, id: &str, thread: Option<&str>, text: &str) {
        self.inject.send(message(chat, id, thread, text)).await.unwrap();
    }

    /// The asker mentions the bot in the group chat.
    async fn say(&self, id: &str, thread: Option<&str>, text: &str) {
        self.say_in(CHAT, id, thread, text).await;
    }

    /// `sender` writes in a thread of the group chat, to `addressee`.
    async fn say_as(&self, sender: &str, id: &str, thread: &str, addressee: Addressee, text: &str) {
        let message = group_message(CHAT, id, Some(thread), sender, addressee, text);
        self.inject.send(message).await.unwrap();
    }

    /// The asker writes in the direct chat.
    async fn say_directly(&self, id: &str, text: &str) {
        let message = Inbound::Message {
            id: MessageId(id.into()),
            at: direct(),
            chat: ChatKind::Direct,
            addressee: Addressee::Bot,
            sender: UserId(ASKER.into()),
            text: text.into(),
        };
        self.inject.send(message).await.unwrap();
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
    async fn posted(&mut self, to: &Destination, message: Post) -> MessageRef {
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

    /// Expect message `id` in `at` acknowledged.
    async fn acknowledged(&mut self, at: &Destination, id: &str) {
        let expected = MessageRef {
            at: at.clone(),
            id: MessageId(id.into()),
        };
        match self.next().await {
            Sent::Acknowledged(actual) => assert_eq!(actual, expected),
            other => panic!("expected an acknowledgement of {id}, got {other:?}"),
        }
    }

    async fn updated(&mut self, posted: &MessageRef, message: Replacement) {
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
                message: Post::ApprovalRequest { id, card },
                posted,
            } => {
                assert_eq!(&actual, to);
                assert_eq!(card, echo_card());
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
        self.sessions
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
        thread: Some(ThreadId(root.into())),
    }
}

/// The conversation that is the direct chat with the asker.
fn direct_key() -> SessionKey {
    SessionKey {
        platform: PLATFORM.into(),
        conversation: ConversationId(DIRECT.into()),
        thread: None,
    }
}

/// The IDs of the messages pending in `backlog`, in the order kept.
async fn pending(backlog: &MemoryBacklog) -> Vec<String> {
    let pending = backlog.pending().await.unwrap();
    pending.into_iter().map(|(key, _)| key.message.0).collect()
}

/// Keep `event` in `backlog` as the channel `PLATFORM` does as it receives it: what a server
/// that ended before handling it leaves behind.
async fn kept_before(backlog: &Arc<MemoryBacklog>, event: &Inbound) {
    let (unread, _) = mpsc::channel(1);
    let inlet = Inlet::new(PLATFORM, unread, backlog.clone());
    assert_eq!(inlet.keep(event).await.unwrap(), Kept::New);
}

/// What the card of an `echo` call shows.
fn echo_card() -> ApprovalCard {
    ApprovalCard {
        tool: "echo".into(),
        summary: r#"{"text":"done"}"#.into(),
        reason: "echo needs approval".into(),
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
    assert_eq!(h.requests(), [["user:U-asker: hello"]]);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_reply_in_the_thread_continues_its_session() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one"), FauxStep::text("two")],
        ..Setup::default()
    });
    let working: Post = text(&h.replies.working.clone());
    h.say("m1", None, "first").await;
    let status = h.posted(&thread("m1"), working.clone()).await;
    h.updated(&status, text("one")).await;
    h.say("m2", Some("m1"), "second").await;
    let status = h.posted(&thread("m1"), working).await;
    h.updated(&status, text("two")).await;
    assert_eq!(
        h.requests()[1],
        ["user:U-asker: first", "assistant:one", "user:U-asker: second"]
    );
    assert_eq!(h.sessions.opened.load(Ordering::SeqCst), 1);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn each_thread_has_a_session_of_its_own() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one"), FauxStep::text("two")],
        ..Setup::default()
    });
    let working: Post = text(&h.replies.working.clone());
    h.say("m1", None, "first").await;
    let status = h.posted(&thread("m1"), working.clone()).await;
    h.updated(&status, text("one")).await;
    h.say("m2", None, "unrelated").await;
    let status = h.posted(&thread("m2"), working).await;
    h.updated(&status, text("two")).await;
    assert_eq!(h.requests()[1], ["user:U-asker: unrelated"]);
    assert_eq!(h.sessions.opened.load(Ordering::SeqCst), 2);
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
    assert_eq!(h.sessions.opened.load(Ordering::SeqCst), 0);
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
        Replacement::ApprovalResolved {
            card: echo_card(),
            id: id.clone(),
            user: Some(UserId(APPROVER.into())),
            result: ApprovalResult::AllowOnce,
        },
    )
    .await;
    h.updated(&status, text("finished")).await;
    assert_eq!(h.sessions.echo.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        h.requests()[1],
        ["user:U-asker: run echo", "assistant:calls[c1]", "tool:c1=done"]
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
        Replacement::ApprovalResolved {
            card: echo_card(),
            id,
            user: Some(UserId(APPROVER.into())),
            result: ApprovalResult::Denied,
        },
    )
    .await;
    h.updated(&status, text("not run")).await;
    assert_eq!(h.sessions.echo.calls.load(Ordering::SeqCst), 0);
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
        Replacement::ApprovalResolved {
            card: echo_card(),
            id,
            user: None,
            result: ApprovalResult::TimedOut,
        },
    )
    .await;
    h.updated(&status, text("gave up")).await;
    assert_eq!(h.sessions.echo.calls.load(Ordering::SeqCst), 0);
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
        Replacement::ApprovalResolved {
            card: echo_card(),
            id,
            user: None,
            result: ApprovalResult::Cancelled,
        },
    )
    .await;
    h.updated(&status, text(&h.replies.cancelled.clone())).await;
    assert_eq!(h.sessions.echo.calls.load(Ordering::SeqCst), 0);
    assert_eq!(h.requests().len(), 1);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn cancel_with_nothing_running_says_so() {
    let mut h = start(Setup::default());
    h.say("m1", None, "/cancel").await;
    h.posted(&thread("m1"), text(&h.replies.nothing_to_cancel.clone()))
        .await;
    assert_eq!(h.sessions.opened.load(Ordering::SeqCst), 0);
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
    let working: Post = text(&h.replies.working.clone());
    h.say("a", None, "task a").await;
    let status_a = h.posted(&thread("a"), working.clone()).await;
    // Thread a now waits for an approval while holding the only run slot.
    let (id, card) = h.card(&thread("a")).await;
    h.say("b", None, "task b").await;
    h.posted(&thread("b"), text(&h.replies.queued.clone())).await;
    h.click("a", APPROVER, &id, Choice::AllowOnce).await;
    h.updated(
        &card,
        Replacement::ApprovalResolved {
            card: echo_card(),
            id,
            user: Some(UserId(APPROVER.into())),
            result: ApprovalResult::AllowOnce,
        },
    )
    .await;
    h.updated(&status_a, text("a done")).await;
    let status_b = h.posted(&thread("b"), working).await;
    h.updated(&status_b, text("b done")).await;
    assert_eq!(h.requests()[2], ["user:U-asker: task b"]);
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
    h.acknowledged(&thread("m1"), "m2").await;
    h.click("m1", APPROVER, &id, Choice::AllowOnce).await;
    h.updated(
        &card,
        Replacement::ApprovalResolved {
            card: echo_card(),
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
            "user:U-asker: run echo",
            "assistant:calls[c1]",
            "tool:c1=done",
            "user:U-asker: also check the logs"
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
    let working: Post = text(&h.replies.working.clone());
    h.say("m1", None, "first").await;
    let status = h.posted(&thread("m1"), working.clone()).await;
    h.updated(&status, text("one")).await;
    assert_eq!(h.event().await, ServerEvent::Released { key: key("m1") });
    h.say("m2", Some("m1"), "second").await;
    let status = h.posted(&thread("m1"), working).await;
    h.updated(&status, text("two")).await;
    assert_eq!(h.sessions.opened.load(Ordering::SeqCst), 2);
    assert_eq!(
        h.requests()[1],
        ["user:U-asker: first", "assistant:one", "user:U-asker: second"]
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_run_cancelled_by_the_stop_says_the_server_is_stopping() {
    let mut h = start(Setup {
        steps: vec![FauxStep::Hang],
        ..Setup::default()
    });
    h.say("m1", None, "wait forever").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working.clone())).await;
    h.cancel.cancel();
    h.updated(&status, text(&h.replies.stopped.clone())).await;
    h.server.await.unwrap().unwrap();
}

#[tokio::test]
async fn stopping_leaves_a_message_waiting_for_a_run_slot_in_the_backlog() {
    let mut h = start(Setup {
        steps: vec![FauxStep::Hang],
        config: ServerConfig {
            max_running: NonZeroUsize::MIN,
            ..Setup::default().config
        },
        ..Setup::default()
    });
    let replies = h.replies.clone();
    let backlog = h.backlog.clone();
    h.say("a", None, "task a").await;
    let status_a = h.posted(&thread("a"), text(&replies.working)).await;
    h.say("b", None, "task b").await;
    h.posted(&thread("b"), text(&replies.queued)).await;
    let (sent, events) = h.stop_and_take().await;
    // The run had taken in all its input, so its thread only learns that the stop cancelled
    // it. Thread b never ran: its message waits for the next start.
    assert_eq!(
        sent,
        [Sent::Update {
            posted: status_a,
            message: text(&replies.stopped)
        }]
    );
    assert!(events.is_empty(), "{events:?}");
    assert_eq!(pending(&backlog).await, ["b"]);
}

#[tokio::test]
async fn stopping_leaves_steering_its_run_did_not_take_in_in_the_backlog() {
    let mut h = start(Setup {
        steps: vec![echo_call()],
        ..Setup::default()
    });
    let replies = h.replies.clone();
    let backlog = h.backlog.clone();
    h.say("m1", None, "run echo").await;
    let status = h.posted(&thread("m1"), text(&replies.working)).await;
    let (id, card) = h.card(&thread("m1")).await;
    h.say("m2", Some("m1"), "also check the logs").await;
    h.acknowledged(&thread("m1"), "m2").await;
    // The worker answers this click only after it has steered the message before it.
    h.click("m1", OTHER, &id, Choice::AllowOnce).await;
    h.posted(&thread("m1"), text(&replies.not_approver)).await;
    let (sent, events) = h.stop_and_take().await;
    let closed = Replacement::ApprovalResolved {
        card: echo_card(),
        id,
        user: None,
        result: ApprovalResult::Cancelled,
    };
    assert_eq!(
        sent,
        [
            Sent::Update {
                posted: card,
                message: closed
            },
            Sent::Update {
                posted: status,
                message: text(&replies.stopped)
            }
        ]
    );
    assert!(matches!(events.as_slice(), [ServerEvent::Approval(_)]), "{events:?}");
    // The run recorded its prompt; the steering it never took waits for the next start.
    assert_eq!(pending(&backlog).await, ["m2"]);
}

#[tokio::test]
async fn a_message_received_as_the_server_stops_stays_in_the_backlog() {
    let h = start(Setup {
        late: vec![message(CHAT, "m1", None, "hello")],
        ..Setup::default()
    });
    let sessions = Arc::clone(&h.sessions);
    let backlog = h.backlog.clone();
    let (sent, events) = h.stop_and_take().await;
    assert!(sent.is_empty(), "{sent:?}");
    assert!(events.is_empty(), "{events:?}");
    assert_eq!(
        sessions.opened.load(Ordering::SeqCst),
        0,
        "nothing runs during the stop"
    );
    assert_eq!(pending(&backlog).await, ["m1"]);
}

#[tokio::test]
async fn a_message_an_earlier_server_left_in_the_backlog_is_answered() {
    let backlog = Arc::new(MemoryBacklog::default());
    kept_before(&backlog, &message(CHAT, "m1", None, "left behind")).await;
    let mut h = start(Setup {
        steps: vec![FauxStep::text("answer"), FauxStep::text("answer")],
        backlog: backlog.clone(),
        ..Setup::default()
    });
    // Delivered again by the platform, it is known and not answered twice.
    h.say("m1", None, "left behind").await;
    h.say("m2", None, "new").await;
    // The two threads may run at the same time.
    let mut answers = 0;
    while answers < 2 {
        if let Sent::Update { message, .. } = h.next().await {
            assert_eq!(message, text("answer"));
            answers += 1;
        }
    }
    let mut requests = h.requests();
    requests.sort();
    assert_eq!(requests, [["user:U-asker: left behind"], ["user:U-asker: new"]]);
    let (sent, events) = h.stop_and_take().await;
    assert!(sent.is_empty(), "{sent:?}");
    assert!(events.is_empty(), "{events:?}");
    assert!(pending(&backlog).await.is_empty());
}

#[tokio::test]
async fn a_handled_message_delivered_again_is_not_answered_again() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one"), FauxStep::text("two")],
        ..Setup::default()
    });
    let working: Post = text(&h.replies.working.clone());
    h.say("m1", None, "first").await;
    let status = h.posted(&thread("m1"), working.clone()).await;
    h.updated(&status, text("one")).await;
    h.say("m1", None, "first").await;
    h.say("m2", Some("m1"), "second").await;
    let status = h.posted(&thread("m1"), working).await;
    h.updated(&status, text("two")).await;
    assert_eq!(h.requests().len(), 2);
    let backlog = h.backlog.clone();
    h.stop().await.unwrap();
    assert!(pending(&backlog).await.is_empty());
}

#[tokio::test]
async fn messages_answered_without_a_run_leave_the_backlog() {
    let mut h = start(Setup::default());
    let replies = h.replies.clone();
    h.say("m1", None, "/cancel").await;
    h.posted(&thread("m1"), text(&replies.nothing_to_cancel)).await;
    h.say_in("C-other", "m2", None, "hello").await;
    let elsewhere = Destination {
        conversation: ConversationId("C-other".into()),
        thread: Some(ThreadId("m2".into())),
    };
    h.posted(&elsewhere, text(&replies.unauthorized)).await;
    let backlog = h.backlog.clone();
    h.stop().await.unwrap();
    assert!(pending(&backlog).await.is_empty());
}

/// A status that an earlier process left in thread `root`.
fn left_in(root: &str, id: &str) -> MessageRef {
    MessageRef {
        at: thread(root),
        id: MessageId(id.into()),
    }
}

fn store_failed(root: &str, doing: &str) -> ServerEvent {
    ServerEvent::Failed {
        key: Some(key(root)),
        error: format!("cannot {doing}: {STORE_ERROR}"),
    }
}

#[tokio::test]
async fn a_running_status_is_kept_and_forgotten_before_the_answer_replaces_it() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("answer")],
        statuses: Some(Vec::new()),
        ..Setup::default()
    });
    h.say("m1", None, "hello").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working.clone())).await;
    assert_eq!(h.next().await, Sent::Kept(status.clone()));
    assert_eq!(h.next().await, Sent::Forgot(status.clone()));
    h.updated(&status, text("answer")).await;
    let statuses = h.statuses.clone().unwrap();
    h.stop().await.unwrap();
    assert!(statuses.kept().is_empty());
}

#[tokio::test]
async fn a_status_left_by_an_earlier_process_is_replaced_and_forgotten() {
    let left = left_in("m0", "s0");
    let mut h = start(Setup {
        statuses: Some(vec![(key("m0"), left.clone())]),
        ..Setup::default()
    });
    h.updated(&left, text(&h.replies.interrupted.clone())).await;
    assert_eq!(h.next().await, Sent::Forgot(left));
    let statuses = h.statuses.clone().unwrap();
    h.stop().await.unwrap();
    assert!(statuses.kept().is_empty());
}

#[tokio::test]
async fn a_left_status_the_platform_does_not_replace_is_reported_and_forgotten() {
    let left = left_in("m0", "s0");
    let mut h = start(Setup {
        statuses: Some(vec![(key("m0"), left.clone())]),
        updates: Updates::Refused,
        ..Setup::default()
    });
    h.updated(&left, text(&h.replies.interrupted.clone())).await;
    assert_eq!(h.next().await, Sent::Forgot(left));
    assert_eq!(
        h.event().await,
        ServerEvent::Failed {
            key: Some(key("m0")),
            error: "cannot replace status s0 that an earlier process left: update refused".into(),
        }
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_left_status_of_a_platform_not_served_is_reported_and_forgotten() {
    let gone = SessionKey {
        platform: "gone".into(),
        ..key("m0")
    };
    let left = left_in("m0", "s0");
    let mut h = start(Setup {
        statuses: Some(vec![(gone.clone(), left.clone())]),
        ..Setup::default()
    });
    assert_eq!(h.next().await, Sent::Forgot(left));
    assert_eq!(
        h.event().await,
        ServerEvent::Failed {
            key: Some(gone),
            error: "cannot replace status s0 that an earlier process left: no channel is named \"gone\"".into(),
        }
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn the_stop_gives_up_replacing_left_statuses_and_keeps_them() {
    let left = left_in("m0", "s0");
    let mut h = start(Setup {
        statuses: Some(vec![(key("m0"), left.clone())]),
        updates: Updates::Hang,
        ..Setup::default()
    });
    // The platform never answers this update.
    h.updated(&left, text(&h.replies.interrupted.clone())).await;
    let statuses = h.statuses.clone().unwrap();
    h.stop().await.unwrap();
    assert_eq!(statuses.kept(), [(key("m0"), left)]);
}

#[tokio::test]
async fn a_status_that_cannot_be_kept_is_reported_and_the_run_goes_on() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("answer")],
        statuses: Some(Vec::new()),
        broken: Some(Broken::Insert),
        ..Setup::default()
    });
    h.say("m1", None, "hello").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working.clone())).await;
    assert_eq!(h.next().await, Sent::Kept(status.clone()));
    assert_eq!(
        h.event().await,
        store_failed("m1", &format!("keep status {}", status.id))
    );
    assert_eq!(h.next().await, Sent::Forgot(status.clone()));
    h.updated(&status, text("answer")).await;
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_status_that_cannot_be_forgotten_is_reported_and_still_replaced() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("answer")],
        statuses: Some(Vec::new()),
        broken: Some(Broken::Remove),
        ..Setup::default()
    });
    h.say("m1", None, "hello").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working.clone())).await;
    assert_eq!(h.next().await, Sent::Kept(status.clone()));
    assert_eq!(h.next().await, Sent::Forgot(status.clone()));
    assert_eq!(
        h.event().await,
        store_failed("m1", &format!("forget status {}", status.id))
    );
    h.updated(&status, text("answer")).await;
    h.stop().await.unwrap();
}

#[tokio::test]
async fn statuses_that_cannot_be_read_are_reported_and_the_server_serves() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("answer")],
        statuses: Some(Vec::new()),
        broken: Some(Broken::List),
        ..Setup::default()
    });
    assert_eq!(
        h.event().await,
        ServerEvent::Failed {
            key: None,
            error: format!("cannot read the statuses an earlier process left: {STORE_ERROR}"),
        }
    );
    h.say("m1", None, "hello").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working.clone())).await;
    assert_eq!(h.next().await, Sent::Kept(status.clone()));
    assert_eq!(h.next().await, Sent::Forgot(status.clone()));
    h.updated(&status, text("answer")).await;
    h.stop().await.unwrap();
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
async fn a_conversation_whose_session_is_gone_goes_on_in_a_new_one() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one"), FauxStep::text("two")],
        config: ServerConfig {
            idle: Duration::from_millis(20),
            ..Setup::default().config
        },
        ..Setup::default()
    });
    let working: Post = text(&h.replies.working.clone());
    h.say_directly("d1", "hello").await;
    let status = h.posted(&direct(), working.clone()).await;
    h.updated(&status, text("one")).await;
    assert_eq!(h.event().await, ServerEvent::Released { key: direct_key() });
    let gone = h.sessions.of(&direct_key()).await.unwrap().remove(0);
    h.sessions.forget(&gone);

    h.say_directly("d2", "again").await;

    match h.event().await {
        ServerEvent::Failed { error, .. } => assert!(error.contains(&gone), "{error}"),
        other => panic!("expected the lost session to be reported, got {other:?}"),
    }
    let status = h.posted(&direct(), working).await;
    h.updated(&status, text("two")).await;
    assert_eq!(h.requests()[1], ["user:again"]);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_session_that_cannot_be_opened_is_reported_and_tried_again() {
    let mut h = start(Setup {
        fail_open: Some("disk full".into()),
        ..Setup::default()
    });
    let refused: Post = text(&format!(
        "{}cannot set up the session: disk full",
        h.replies.cannot_open
    ));
    h.say("m1", None, "hello").await;
    h.posted(&thread("m1"), refused.clone()).await;
    match h.event().await {
        ServerEvent::Failed { key: Some(key), error } => {
            assert_eq!(key.thread, Some(ThreadId("m1".into())));
            assert!(error.contains("disk full"), "{error}");
        }
        other => panic!("expected a failure, got {other:?}"),
    }
    h.say("m2", Some("m1"), "again").await;
    h.posted(&thread("m1"), refused).await;
    assert_eq!(h.sessions.opened.load(Ordering::SeqCst), 2);
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
            updates: Updates::default(),
            history: Vec::new(),
            history_gate: None,
            names: HashMap::new(),
        })
    };
    let h = start(Setup::default());
    let server = Server::new(ServerConfig::default(), h.agent.clone(), Arc::new(NullServerSink));
    let result = server.run(vec![channel(), channel()], &CancellationToken::new()).await;
    assert!(matches!(result, Err(ImError::Config(error)) if error.contains("fake")));
    h.stop().await.unwrap();
}

struct NullServerSink;

impl ServerSink for NullServerSink {
    fn emit(&self, _event: &ServerEvent) {}
}

#[tokio::test]
async fn a_direct_chat_is_one_conversation_answered_in_the_chat() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one"), FauxStep::text("two")],
        ..Setup::default()
    });
    let working: Post = text(&h.replies.working.clone());
    h.say_directly("d1", "first").await;
    let status = h.posted(&direct(), working.clone()).await;
    h.updated(&status, text("one")).await;
    h.say_directly("d2", "second").await;
    let status = h.posted(&direct(), working).await;
    h.updated(&status, text("two")).await;
    // One person: no names before their words.
    assert_eq!(h.requests()[1], ["user:first", "assistant:one", "user:second"]);
    assert_eq!(h.sessions.opened.load(Ordering::SeqCst), 1);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn the_session_records_the_sender_of_each_message_it_answers_as_its_actor() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one"), FauxStep::text("two")],
        ..Setup::default()
    });
    let working: Post = text(&h.replies.working.clone());
    h.say("m1", None, "first").await;
    let status = h.posted(&thread("m1"), working.clone()).await;
    h.updated(&status, text("one")).await;
    h.say_as(OTHER, "m2", "m1", Addressee::Bot, "second").await;
    let status = h.posted(&thread("m1"), working).await;
    h.updated(&status, text("two")).await;

    let key = SessionKey {
        platform: PLATFORM.into(),
        conversation: ConversationId(CHAT.into()),
        thread: Some(ThreadId("m1".into())),
    };
    let id = h.sessions.of(&key).await.unwrap().remove(0);
    let stored = Transcript::snapshot(h.sessions.storage.read(&id).await.unwrap()).unwrap();
    let actors: Vec<Option<&str>> = stored
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            Entry::Message(entry) if matches!(entry.message, Message::User { .. }) => Some(entry.actor.as_deref()),
            _ => None,
        })
        .collect();
    assert_eq!(actors, [Some(ASKER), Some(OTHER)]);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn new_starts_a_new_session_and_keeps_the_earlier_one() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one"), FauxStep::text("two")],
        ..Setup::default()
    });
    let working: Post = text(&h.replies.working.clone());
    h.say_directly("d1", "first").await;
    let status = h.posted(&direct(), working.clone()).await;
    h.updated(&status, text("one")).await;
    h.say_directly("d2", " /new ").await;
    h.posted(&direct(), text(&h.replies.new_session.clone())).await;
    h.say_directly("d3", "second").await;
    let status = h.posted(&direct(), working).await;
    h.updated(&status, text("two")).await;
    assert_eq!(h.requests()[1], ["user:second"]);
    assert_eq!(h.sessions.of(&direct_key()).await.unwrap().len(), 2);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_conversation_released_after_new_goes_on_in_the_new_session() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one"), FauxStep::text("two")],
        config: ServerConfig {
            idle: Duration::from_millis(20),
            ..Setup::default().config
        },
        ..Setup::default()
    });
    let working: Post = text(&h.replies.working.clone());
    let released = ServerEvent::Released { key: direct_key() };
    h.say_directly("d1", "first").await;
    let status = h.posted(&direct(), working.clone()).await;
    h.updated(&status, text("one")).await;
    // The idle time passes before `/new`, so the new session is later than the first one's
    // last record by more than the clock's resolution.
    assert_eq!(h.event().await, released);
    h.say_directly("d2", "/new").await;
    h.posted(&direct(), text(&h.replies.new_session.clone())).await;
    // Released again, the conversation remembers nothing: only the new session, which it
    // held until now, says that `/new` was asked for.
    assert_eq!(h.event().await, released);
    h.say_directly("d3", "second").await;
    let status = h.posted(&direct(), working).await;
    h.updated(&status, text("two")).await;
    assert_eq!(h.requests()[1], ["user:second"]);
    assert_eq!(h.sessions.of(&direct_key()).await.unwrap().len(), 2);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn new_reports_a_session_it_cannot_create() {
    let mut h = start(Setup {
        fail_open: Some("disk full".into()),
        ..Setup::default()
    });
    h.say_directly("d1", "/new").await;
    let refused = format!("{}cannot set up the session: disk full", h.replies.cannot_open);
    h.posted(&direct(), text(&refused)).await;
    match h.event().await {
        ServerEvent::Failed { key: Some(key), error } => {
            assert_eq!(key, direct_key());
            assert!(error.contains("disk full"), "{error}");
        }
        other => panic!("expected a failure, got {other:?}"),
    }
    assert!(h.sessions.of(&direct_key()).await.unwrap().is_empty());
    h.stop().await.unwrap();
}

#[tokio::test]
async fn new_during_a_run_cancels_it_and_then_starts_over() {
    let mut h = start(Setup {
        steps: vec![echo_call(), FauxStep::text("fresh")],
        ..Setup::default()
    });
    h.say_directly("d1", "run echo").await;
    let status = h.posted(&direct(), text(&h.replies.working.clone())).await;
    // The card shows that the run is waiting, its request answered.
    let (id, card) = h.card(&direct()).await;
    h.say_directly("d2", "/new").await;
    let closed = Replacement::ApprovalResolved {
        card: echo_card(),
        id,
        user: None,
        result: ApprovalResult::Cancelled,
    };
    h.updated(&card, closed).await;
    h.updated(&status, text(&h.replies.cancelled.clone())).await;
    h.posted(&direct(), text(&h.replies.new_session.clone())).await;
    h.say_directly("d3", "again").await;
    let status = h.posted(&direct(), text(&h.replies.working.clone())).await;
    h.updated(&status, text("fresh")).await;
    assert_eq!(h.requests()[1], ["user:again"]);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_message_that_arrives_while_new_cancels_a_run_is_answered_in_the_new_session() {
    let release = CancellationToken::new();
    let mut h = start(Setup {
        steps: vec![echo_call(), FauxStep::text("fresh")],
        updates: Updates::Held(release.clone()),
        ..Setup::default()
    });
    let replies = h.replies.clone();
    h.say_directly("d1", "run echo").await;
    let status = h.posted(&direct(), text(&replies.working)).await;
    let (id, card) = h.card(&direct()).await;
    h.say_directly("d2", "/new").await;
    // The cancelled run closes its card, and the platform holds that update back: the
    // conversation is busy with it, its run not yet seen to have ended.
    let closed = Replacement::ApprovalResolved {
        card: echo_card(),
        id,
        user: None,
        result: ApprovalResult::Cancelled,
    };
    h.updated(&card, closed).await;
    h.say_directly("d3", "again").await;
    // The router passes messages on in the order it got them: once it has refused this one,
    // the one before it waits in the conversation's inbox.
    h.say_in("C2", "m1", None, "hello").await;
    let elsewhere = Destination {
        conversation: ConversationId("C2".into()),
        thread: Some(ThreadId("m1".into())),
    };
    h.posted(&elsewhere, text(&replies.unauthorized)).await;
    release.cancel();
    h.acknowledged(&direct(), "d3").await;
    h.updated(&status, text(&replies.cancelled)).await;
    h.posted(&direct(), text(&replies.new_session)).await;
    let status = h.posted(&direct(), text(&replies.working)).await;
    h.updated(&status, text("fresh")).await;
    assert_eq!(h.requests()[1], ["user:again"]);
    assert_eq!(h.sessions.of(&direct_key()).await.unwrap().len(), 2);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_session_closed_under_its_conversation_is_opened_again_for_the_next_message() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one"), FauxStep::text("two")],
        ..Setup::default()
    });
    let working: Post = text(&h.replies.working.clone());
    h.say_directly("d1", "first").await;
    let status = h.posted(&direct(), working.clone()).await;
    h.updated(&status, text("one")).await;
    // Closed by someone other than the conversation, which still holds it: a session also
    // closes itself this way when it can no longer write its records.
    let id = h.sessions.of(&direct_key()).await.unwrap().remove(0);
    assert!(h.agent.close_session(&id));
    h.say_directly("d2", "second").await;
    let status = h.posted(&direct(), working).await;
    h.updated(&status, text("two")).await;
    // The message that found the session closed is answered in the session opened again.
    assert_eq!(h.requests()[1], ["user:first", "assistant:one", "user:second"]);
    assert_eq!(h.sessions.opened.load(Ordering::SeqCst), 2);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_session_closed_during_its_run_is_let_go_and_opened_again_for_the_next_message() {
    let mut h = start(Setup {
        steps: vec![echo_call(), FauxStep::text("two")],
        ..Setup::default()
    });
    let replies = h.replies.clone();
    h.say_directly("d1", "run echo").await;
    let status = h.posted(&direct(), text(&replies.working)).await;
    // The run waits for an approval, so it is in progress when its session is closed.
    let (id, card) = h.card(&direct()).await;
    let session = h.sessions.of(&direct_key()).await.unwrap().remove(0);
    assert!(h.agent.close_session(&session));
    let closed = Replacement::ApprovalResolved {
        card: echo_card(),
        id,
        user: None,
        result: ApprovalResult::Cancelled,
    };
    h.updated(&card, closed).await;
    h.updated(&status, text(&replies.cancelled)).await;
    // The approval that the closing cancelled is reported before the release.
    while h.event().await != (ServerEvent::Released { key: direct_key() }) {}
    h.say_directly("d2", "again").await;
    let status = h.posted(&direct(), text(&replies.working)).await;
    h.updated(&status, text("two")).await;
    assert_eq!(h.requests()[1].last().map(String::as_str), Some("user:again"));
    assert_eq!(h.sessions.opened.load(Ordering::SeqCst), 2);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn talk_in_a_thread_the_bot_never_joined_reaches_neither_a_worker_nor_the_storage() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("hi")],
        ..Setup::default()
    });
    for id in ["m2", "m3", "m4"] {
        h.say_as(OTHER, id, "m1", Addressee::Nobody, "talk among people").await;
    }
    // A mention in the direct chat, answered once the router has passed the talk by: it
    // routes what arrives in order.
    h.say_directly("d1", "hello").await;
    let status = h.posted(&direct(), text(&h.replies.working.clone())).await;
    h.updated(&status, text("hi")).await;

    assert_eq!(h.sessions.indexed.load(Ordering::SeqCst), 1, "read once, at the start");
    assert_eq!(h.sessions.listed.load(Ordering::SeqCst), 0);
    assert_eq!(h.sessions.opened.load(Ordering::SeqCst), 1, "the direct chat's session");
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_conversation_of_an_earlier_process_is_followed_without_a_mention() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("first")],
        ..Setup::default()
    });
    h.say_as(ASKER, "m2", "m1", Addressee::Bot, "first").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working.clone())).await;
    h.updated(&status, text("first")).await;
    let storage = Arc::clone(&h.sessions.storage);
    h.stop().await.unwrap();

    // A new server over the same storage, as after a restart.
    let mut h = start(Setup {
        steps: vec![FauxStep::text("second")],
        storage: Some(storage),
        ..Setup::default()
    });
    h.say_as(OTHER, "m3", "m1", Addressee::Nobody, "said while the bot was away")
        .await;
    h.say_as(ASKER, "m4", "m1", Addressee::Bot, "again").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working.clone())).await;
    h.updated(&status, text("second")).await;
    let request = &h.requests()[0];
    assert!(
        request
            .iter()
            .any(|message| message.contains("said while the bot was away")),
        "{request:?}"
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_thread_the_bot_joins_after_talk_it_passed_by_is_followed_from_then_on() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one"), FauxStep::text("two")],
        config: ServerConfig {
            idle: Duration::from_millis(20),
            ..Setup::default().config
        },
        ..Setup::default()
    });
    h.say_as(OTHER, "m2", "m1", Addressee::Nobody, "before the bot").await;
    h.say_as(ASKER, "m3", "m1", Addressee::Bot, "first").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working.clone())).await;
    h.updated(&status, text("one")).await;
    while h.event().await != (ServerEvent::Released { key: key("m1") }) {}

    // The bot took part now: talk there that does not mention it reaches the conversation,
    // as context its next answer reads, whether the router still has the thread's worker or
    // looks the thread up again.
    h.say_as(OTHER, "m4", "m1", Addressee::Nobody, "the cache was flushed")
        .await;
    h.say_as(ASKER, "m5", "m1", Addressee::Bot, "second").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working.clone())).await;
    h.updated(&status, text("two")).await;
    let request = &h.requests()[1];
    assert!(
        request.iter().any(|message| message.contains("the cache was flushed")),
        "{request:?}"
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn what_was_asked_during_a_run_whose_session_closed_is_answered_in_the_session_opened_again() {
    let mut h = start(Setup {
        steps: vec![echo_call(), FauxStep::text("two")],
        ..Setup::default()
    });
    let replies = h.replies.clone();
    h.say_directly("d1", "run echo").await;
    let status = h.posted(&direct(), text(&replies.working)).await;
    let (id, card) = h.card(&direct()).await;
    // Asked while the run waits for its approval: acknowledged, and queued for the run.
    h.say_directly("d2", "meanwhile").await;
    h.acknowledged(&direct(), "d2").await;
    // Closed under the conversation, as a session closes itself when it can no longer write
    // its records: the run ends without taking what was queued.
    let session = h.sessions.of(&direct_key()).await.unwrap().remove(0);
    assert!(h.agent.close_session(&session));
    let closed = Replacement::ApprovalResolved {
        card: echo_card(),
        id,
        user: None,
        result: ApprovalResult::Cancelled,
    };
    h.updated(&card, closed).await;
    h.updated(&status, text(&replies.cancelled)).await;

    let status = h.posted(&direct(), text(&replies.working)).await;
    h.updated(&status, text("two")).await;
    assert_eq!(h.requests()[1].last().map(String::as_str), Some("user:meanwhile"));
    assert_eq!(h.sessions.opened.load(Ordering::SeqCst), 2);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn new_starts_its_session_also_when_the_one_it_leaves_was_closed_meanwhile() {
    let release = CancellationToken::new();
    let mut h = start(Setup {
        steps: vec![echo_call(), FauxStep::text("fresh")],
        updates: Updates::Held(release.clone()),
        ..Setup::default()
    });
    let replies = h.replies.clone();
    h.say_directly("d1", "run echo").await;
    let status = h.posted(&direct(), text(&replies.working)).await;
    let (id, card) = h.card(&direct()).await;
    h.say_directly("d2", "/new").await;
    let closed = Replacement::ApprovalResolved {
        card: echo_card(),
        id,
        user: None,
        result: ApprovalResult::Cancelled,
    };
    h.updated(&card, closed).await;
    // Closed under the conversation while its cancelled run ends, as a session closes itself
    // when it can no longer write its records.
    let left = h.sessions.of(&direct_key()).await.unwrap().remove(0);
    assert!(h.agent.close_session(&left));
    release.cancel();
    h.updated(&status, text(&replies.cancelled)).await;
    h.posted(&direct(), text(&replies.new_session)).await;
    h.say_directly("d3", "again").await;
    let status = h.posted(&direct(), text(&replies.working)).await;
    h.updated(&status, text("fresh")).await;
    assert_eq!(h.requests()[1], ["user:again"]);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn the_person_just_answered_goes_on_without_a_mention() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one"), FauxStep::text("two")],
        ..Setup::default()
    });
    let working: Post = text(&h.replies.working.clone());
    h.say("m1", None, "first").await;
    let status = h.posted(&thread("m1"), working.clone()).await;
    h.updated(&status, text("one")).await;
    h.say_as(ASKER, "m2", "m1", Addressee::Nobody, "and then?").await;
    let status = h.posted(&thread("m1"), working).await;
    h.updated(&status, text("two")).await;
    assert_eq!(
        h.requests()[1],
        ["user:U-asker: first", "assistant:one", "user:U-asker: and then?"]
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn talk_between_people_in_the_thread_is_recorded_but_not_answered() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one"), FauxStep::text("two")],
        names: HashMap::from([(OTHER.to_string(), "Li".to_string())]),
        ..Setup::default()
    });
    let working: Post = text(&h.replies.working.clone());
    h.say("m1", None, "why is checkout slow?").await;
    let status = h.posted(&thread("m1"), working.clone()).await;
    h.updated(&status, text("one")).await;
    // The asker turns to someone else, who answers; then the asker speaks without a mention.
    h.say_as(ASKER, "m2", "m1", Addressee::Others, "@Li any idea?").await;
    h.say_as(OTHER, "m3", "m1", Addressee::Nobody, "the cache was flushed")
        .await;
    h.say_as(ASKER, "m4", "m1", Addressee::Nobody, "makes sense").await;
    // Only a mention brings the bot back; what it did not answer is in its context.
    h.say("m5", Some("m1"), "so what now?").await;
    let status = h.posted(&thread("m1"), working).await;
    h.updated(&status, text("two")).await;
    assert_eq!(
        h.requests()[1],
        [
            "user:U-asker: why is checkout slow?",
            "assistant:one",
            "user:U-asker: @Li any idea?",
            "user:Li: the cache was flushed",
            "user:U-asker: makes sense",
            "user:U-asker: so what now?",
        ]
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn talk_where_the_bot_never_took_part_is_left_alone() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one")],
        ..Setup::default()
    });
    h.say_as(OTHER, "m1", "t1", Addressee::Nobody, "lunch?").await;
    let elsewhere = group_message("C2", "m2", Some("t2"), OTHER, Addressee::Nobody, "lunch?");
    h.inject.send(elsewhere).await.unwrap();
    h.say("m3", None, "hello").await;
    // The first thing the server does is answer the mention: no refusal, no run before it.
    let status = h.posted(&thread("m3"), text(&h.replies.working.clone())).await;
    h.updated(&status, text("one")).await;
    assert_eq!(h.sessions.opened.load(Ordering::SeqCst), 1);
    h.stop().await.unwrap();
}

/// A message that `sender` posted in a thread before the bot was mentioned there.
fn earlier(id: &str, sender: &str, text: &str) -> Said {
    Said {
        id: MessageId(id.into()),
        sender: UserId(sender.into()),
        text: text.into(),
    }
}

#[tokio::test]
async fn the_first_mention_in_a_thread_reads_what_was_said_there() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one")],
        history: vec![earlier("t1", OTHER, "the deploy failed"), earlier("m2", ASKER, "why?")],
        names: HashMap::from([(OTHER.to_string(), "Li".to_string())]),
        ..Setup::default()
    });
    h.say("m2", Some("t1"), "why?").await;
    let status = h.posted(&thread("t1"), text(&h.replies.working.clone())).await;
    h.updated(&status, text("one")).await;
    // The mention itself is the prompt, not part of what came before.
    assert_eq!(h.requests()[0], ["user:Li: the deploy failed", "user:U-asker: why?"]);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn the_first_mention_in_a_thread_reads_only_what_was_said_before_it() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one")],
        // Read once a run slot was free: by then the thread went on after the mention.
        history: vec![
            earlier("t1", OTHER, "the deploy failed"),
            earlier("m2", ASKER, "why?"),
            earlier("m3", OTHER, "any news?"),
        ],
        names: HashMap::from([(OTHER.to_string(), "Li".to_string())]),
        ..Setup::default()
    });
    h.say("m2", Some("t1"), "why?").await;
    let status = h.posted(&thread("t1"), text(&h.replies.working.clone())).await;
    h.updated(&status, text("one")).await;
    assert_eq!(h.requests()[0], ["user:Li: the deploy failed", "user:U-asker: why?"]);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_session_started_by_new_does_not_read_what_was_said_in_the_thread() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one")],
        config: ServerConfig {
            idle: Duration::from_millis(20),
            ..Setup::default().config
        },
        history: vec![earlier("t1", OTHER, "the deploy failed")],
        ..Setup::default()
    });
    h.say("m2", Some("t1"), "/new").await;
    h.posted(&thread("t1"), text(&h.replies.new_session.clone())).await;
    // Released, the conversation opens the session as it would any other of its own.
    assert_eq!(h.event().await, ServerEvent::Released { key: key("t1") });
    h.say("m3", Some("t1"), "why?").await;
    let status = h.posted(&thread("t1"), text(&h.replies.working.clone())).await;
    h.updated(&status, text("one")).await;
    assert_eq!(h.requests()[0], ["user:U-asker: why?"]);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn talk_in_a_thread_after_its_session_was_released_is_still_recorded() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one"), FauxStep::text("two")],
        config: ServerConfig {
            idle: Duration::from_millis(20),
            ..Setup::default().config
        },
        ..Setup::default()
    });
    let working: Post = text(&h.replies.working.clone());
    h.say("m1", None, "first").await;
    let status = h.posted(&thread("m1"), working.clone()).await;
    h.updated(&status, text("one")).await;
    assert_eq!(h.event().await, ServerEvent::Released { key: key("m1") });
    h.say_as(OTHER, "m2", "m1", Addressee::Nobody, "fixed it").await;
    h.say("m3", Some("m1"), "confirm?").await;
    let status = h.posted(&thread("m1"), working).await;
    h.updated(&status, text("two")).await;
    assert_eq!(
        h.requests()[1],
        [
            "user:U-asker: first",
            "assistant:one",
            "user:U-other: fixed it",
            "user:U-asker: confirm?"
        ]
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_slow_session_setup_does_not_block_another_conversations_messages() {
    let gate = Gate::new();
    let mut h = start(Setup {
        steps: vec![FauxStep::text("hi")],
        setup_gate: Some(gate.clone()),
        ..Setup::default()
    });
    h.say_as(ASKER, "m2", "slow-thread", Addressee::Bot, "please").await;
    tokio::time::timeout(WAIT, gate.entered.notified()).await.unwrap();
    h.say_directly("d1", "hello").await;
    // The direct chat is answered while the thread's setup is still held. Only WAIT, the
    // harness's failure bound, limits this wait; no scheduler-speed assumption is needed.
    let status = h.posted(&direct(), text(&h.replies.working.clone())).await;
    h.updated(&status, text("hi")).await;
    gate.release.cancel();
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_message_that_arrives_while_cancel_stops_a_run_is_answered_after_it() {
    let release = CancellationToken::new();
    let mut h = start(Setup {
        steps: vec![echo_call(), FauxStep::text("fresh")],
        updates: Updates::Held(release.clone()),
        ..Setup::default()
    });
    let replies = h.replies.clone();
    h.say_directly("d1", "run echo").await;
    let status = h.posted(&direct(), text(&replies.working)).await;
    let (id, card) = h.card(&direct()).await;
    h.say_directly("d2", "/cancel").await;
    // The cancelled run closes its card, and the platform holds that update back: the
    // conversation is busy with it, its run not yet seen to have ended.
    let closed = Replacement::ApprovalResolved {
        card: echo_card(),
        id,
        user: None,
        result: ApprovalResult::Cancelled,
    };
    h.updated(&card, closed).await;
    h.say_directly("d3", "again").await;
    // The router passes messages on in the order it got them: once it has refused this one,
    // the one before it waits in the conversation's inbox.
    h.say_in("C2", "m1", None, "hello").await;
    let elsewhere = Destination {
        conversation: ConversationId("C2".into()),
        thread: Some(ThreadId("m1".into())),
    };
    h.posted(&elsewhere, text(&replies.unauthorized)).await;
    release.cancel();
    h.acknowledged(&direct(), "d3").await;
    h.updated(&status, text(&replies.cancelled)).await;
    let status = h.posted(&direct(), text(&replies.working)).await;
    h.updated(&status, text("fresh")).await;
    let second = &h.requests()[1];
    assert_eq!(second.last().map(String::as_str), Some("user:again"), "{second:?}");
    h.stop().await.unwrap();
}

#[tokio::test]
async fn cancel_drops_what_was_steered_into_the_run_it_cancels() {
    let mut h = start(Setup {
        steps: vec![FauxStep::Hang],
        ..Setup::default()
    });
    let replies = h.replies.clone();
    let working: Post = text(&replies.working);
    h.say_directly("d1", "run").await;
    let status = h.posted(&direct(), working.clone()).await;
    h.say_directly("d2", "also this").await;
    h.acknowledged(&direct(), "d2").await;
    h.say_directly("d3", "/cancel").await;
    h.updated(&status, text(&replies.cancelled)).await;

    let (sent, _) = h.stop_and_take().await;

    assert!(
        !sent
            .iter()
            .any(|sent| matches!(sent, Sent::Post { message, .. } if *message == working)),
        "no run for the steering the cancelled run did not take in: {sent:?}"
    );
}

#[tokio::test]
async fn input_left_by_a_run_waits_behind_a_conversation_that_queued_first() {
    let mut h = start(Setup {
        steps: vec![echo_call(), FauxStep::text("b done"), FauxStep::text("a done")],
        config: ServerConfig {
            max_running: NonZeroUsize::MIN,
            ..Setup::default().config
        },
        ..Setup::default()
    });
    let replies = h.replies.clone();
    h.say_directly("d1", "run echo").await;
    h.posted(&direct(), text(&replies.working)).await;
    // The run waits for an approval, holding the only slot.
    h.card(&direct()).await;
    h.say("b1", None, "the other thread").await;
    h.posted(&thread("b1"), text(&replies.queued)).await;
    h.say_directly("d2", "more").await;
    h.acknowledged(&direct(), "d2").await;
    // Closing the session ends the run and leaves "more" to answer in the session opened
    // again.
    let session = h.sessions.of(&direct_key()).await.unwrap().remove(0);
    assert!(h.agent.close_session(&session));
    let done: Replacement = text("a done");
    while !matches!(h.next().await, Sent::Update { message, .. } if message == done) {}

    let requests = h.requests();
    assert!(
        requests[1].iter().any(|message| message.contains("the other thread")),
        "the thread that queued first runs first: {requests:?}"
    );
    assert!(
        requests[2].iter().any(|message| message.contains("more")),
        "{requests:?}"
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_first_session_is_made_only_once_its_thread_is_read() {
    let gate = Gate::new();
    let mut h = start(Setup {
        steps: vec![FauxStep::text("hi")],
        history_gate: Some(gate.clone()),
        ..Setup::default()
    });
    h.say_as(ASKER, "m2", "m1", Addressee::Bot, "please").await;
    tokio::time::timeout(WAIT, gate.entered.notified()).await.unwrap();
    assert_eq!(
        h.sessions.opened.load(Ordering::SeqCst),
        0,
        "a session that starts before its thread is read could be left without it"
    );
    gate.release.cancel();
    let status = h.posted(&thread("m1"), text(&h.replies.working.clone())).await;
    h.updated(&status, text("hi")).await;
    h.stop().await.unwrap();
}

#[tokio::test]
async fn talk_after_a_release_waits_for_a_run_slot_to_open_the_session() {
    let gate = Gate::disarmed();
    let mut h = start(Setup {
        steps: vec![FauxStep::text("first"), FauxStep::Hang],
        config: ServerConfig {
            max_running: NonZeroUsize::MIN,
            idle: Duration::from_millis(20),
            ..Setup::default().config
        },
        setup_gate: Some(gate.clone()),
        ..Setup::default()
    });
    let replies = h.replies.clone();
    let released = ServerEvent::Released {
        key: key("slow-thread"),
    };
    // The bot takes part in the thread, whose session is then released.
    h.say_as(ASKER, "m2", "slow-thread", Addressee::Bot, "first").await;
    let status = h.posted(&thread("slow-thread"), text(&replies.working)).await;
    h.updated(&status, text("first")).await;
    while h.event().await != released {}
    gate.arm();
    // The direct chat's run holds the only slot.
    h.say_directly("d1", "run").await;
    let running = h.posted(&direct(), text(&replies.working)).await;

    // Talk in the thread waits for the slot instead of setting up the session at once, so
    // the thread's worker still answers a command meanwhile.
    h.say_as(OTHER, "m3", "slow-thread", Addressee::Nobody, "said meanwhile")
        .await;
    h.say_as(ASKER, "m4", "slow-thread", Addressee::Bot, "/cancel").await;
    h.posted(&thread("slow-thread"), text(&replies.nothing_to_cancel)).await;

    // Once the slot is free, the session is set up and what was said is recorded.
    h.say_directly("d2", "/cancel").await;
    h.updated(&running, text(&replies.cancelled)).await;
    tokio::time::timeout(WAIT, gate.entered.notified()).await.unwrap();
    gate.release.cancel();
    while h.event().await != released {}
    let id = h.sessions.of(&key("slow-thread")).await.unwrap().remove(0);
    let records = h.sessions.storage.read(&id).await.unwrap().records;
    assert!(
        records.iter().any(|record| record.contains("said meanwhile")),
        "{records:?}"
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_conversation_waiting_for_a_run_slot_does_not_set_up_a_session() {
    let mut h = start(Setup {
        steps: vec![FauxStep::Hang],
        config: ServerConfig {
            max_running: NonZeroUsize::new(1).unwrap(),
            ..Setup::default().config
        },
        ..Setup::default()
    });
    h.say("m1", None, "run").await;
    h.posted(&thread("m1"), text(&h.replies.working.clone())).await;
    h.say_directly("d1", "wait").await;
    h.posted(&direct(), text(&h.replies.queued.clone())).await;
    let opened = h.sessions.opened.load(Ordering::SeqCst);
    h.stop().await.unwrap();
    assert_eq!(
        opened, 1,
        "a queued conversation must not start a container or hold a session"
    );
}

#[tokio::test]
async fn the_conversation_limit_refuses_new_workers_before_session_setup() {
    let mut h = start(Setup {
        steps: vec![FauxStep::Hang],
        config: ServerConfig {
            max_conversations: NonZeroUsize::new(1).unwrap(),
            ..Setup::default().config
        },
        ..Setup::default()
    });
    h.say("m1", None, "run").await;
    h.posted(&thread("m1"), text(&h.replies.working.clone())).await;
    h.say_directly("d1", "another conversation").await;
    h.posted(&direct(), text(&h.replies.overloaded.clone())).await;
    assert_eq!(h.sessions.opened.load(Ordering::SeqCst), 1);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_waiting_conversation_bounds_its_messages_and_still_accepts_cancel() {
    let mut h = start(Setup {
        steps: vec![FauxStep::Hang],
        config: ServerConfig {
            max_running: NonZeroUsize::new(1).unwrap(),
            max_pending_messages: NonZeroUsize::new(1).unwrap(),
            ..Setup::default().config
        },
        ..Setup::default()
    });
    h.say("m1", None, "run").await;
    h.posted(&thread("m1"), text(&h.replies.working.clone())).await;
    h.say_directly("d1", "wait").await;
    h.posted(&direct(), text(&h.replies.queued.clone())).await;
    h.say_directly("d2", "too much").await;
    h.posted(&direct(), text(&h.replies.overloaded.clone())).await;
    h.say_directly("d3", "/cancel").await;
    h.posted(&direct(), text(&h.replies.cancelled.clone())).await;
    h.say_directly("d4", "wait again").await;
    h.posted(&direct(), text(&h.replies.queued.clone())).await;
    assert_eq!(h.sessions.opened.load(Ordering::SeqCst), 1);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn talk_refused_for_capacity_gets_no_reply_and_a_request_does() {
    let gate = Gate::new();
    let mut h = start(Setup {
        config: ServerConfig {
            max_pending_messages: NonZeroUsize::new(1).unwrap(),
            ..Setup::default().config
        },
        setup_gate: Some(gate.clone()),
        ..Setup::default()
    });
    let overloaded: Post = text(&h.replies.overloaded);
    h.say_as(ASKER, "m1", "slow-thread", Addressee::Bot, "first").await;
    // The first message keeps its share of the budget while its session is set up.
    tokio::time::timeout(WAIT, gate.entered.notified()).await.unwrap();
    h.say_as(OTHER, "m2", "slow-thread", Addressee::Nobody, "among people")
        .await;
    h.say_as(ASKER, "m3", "slow-thread", Addressee::Bot, "second").await;
    // The first reply is the one to the request: the talk before it got none.
    h.posted(&thread("slow-thread"), overloaded.clone()).await;
    gate.release.cancel();
    let (sent, events) = h.stop_and_take().await;

    assert!(
        !sent
            .iter()
            .any(|sent| matches!(sent, Sent::Post { message, .. } if *message == overloaded)),
        "{sent:?}"
    );
    let refused = events
        .iter()
        .filter(|event| matches!(event, ServerEvent::Failed { error, .. } if error.contains("capacity exceeded")))
        .count();
    assert_eq!(refused, 2, "the operator hears of both: {events:?}");
}

#[tokio::test]
async fn steering_is_bounded_by_utf8_bytes_without_acknowledging_rejected_messages() {
    let mut h = start(Setup {
        steps: vec![FauxStep::Hang],
        config: ServerConfig {
            max_pending_bytes: NonZeroUsize::new(6).unwrap(),
            ..Setup::default().config
        },
        ..Setup::default()
    });
    h.say_directly("d1", "run").await;
    let status = h.posted(&direct(), text(&h.replies.working.clone())).await;
    h.say_directly("d2", "中文").await;
    h.acknowledged(&direct(), "d2").await;
    h.say_directly("d3", "x").await;
    h.posted(&direct(), text(&h.replies.overloaded.clone())).await;
    h.say_directly("d4", "/cancel").await;
    h.updated(&status, text(&h.replies.cancelled.clone())).await;
    h.stop().await.unwrap();
}

#[tokio::test]
async fn an_oversized_message_is_refused_without_opening_a_session() {
    let mut h = start(Setup {
        config: ServerConfig {
            max_pending_bytes: NonZeroUsize::new(5).unwrap(),
            ..Setup::default().config
        },
        ..Setup::default()
    });
    h.say_directly("d1", "中文").await;
    h.posted(&direct(), text(&h.replies.overloaded.clone())).await;
    assert_eq!(h.sessions.opened.load(Ordering::SeqCst), 0);
    h.stop().await.unwrap();
}
