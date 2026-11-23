//! The IM server with an in-memory channel and a scripted model. This file is the harness: a
//! fake channel, the statuses and sessions the server keeps, and a started server to talk to.
//! The tests are in its modules, one topic each:
//! - `conversations`: which conversation a message belongs to and when it is answered
//! - `commands`: `/new`, `/cancel` and `/compact`
//! - `approvals`: approval cards and their clicks
//! - `runs`: the run and conversation limits, steering, and the bounds on waiting messages
//! - `lifecycle`: idle release, the stop, the backlog a restart answers, and sessions that
//!   close or cannot be opened
//! - `statuses`: the statuses of running work kept for a restart
//! - `images`: the images of messages, fetched for the messages the bot answers
//! - `triggers`: conversations the server starts without a message
//! - `chats`: what a chat has of its own: an agent, approvers
//! - `daily`: what the work of a day may use, in all chats and in one
//! - `access`: the chats served and the approvers, replaced while the server runs

mod access;
mod approvals;
mod chats;
mod commands;
mod conversations;
mod daily;
mod images;
mod lifecycle;
mod runs;
mod statuses;
mod triggers;

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
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::{Message, StopReason, Usage};
use zx_agent::builtin::policy::{Rule, RuleMatcher, RulePolicy};
use zx_agent::compaction::{Summarized, Summarizer, SummarySource};
use zx_agent::events::{ApprovalResult, Event, EventKind, RunOutcome};
use zx_agent::hooks::{Hook, HookContext, HookResult};
use zx_agent::policy::{ApprovalMode, PolicyDecision};
use zx_agent::tools::Tool;
use zx_agent::transcript::{
    CompactionSummary, Entry, LoadedSession, MemoryStorage, SessionHeader, SessionList, SessionLog, SessionQuery,
    SessionStorage, SummaryFact, Transcript,
};
use zx_im::{
    Access, Addressee, ApprovalCard, ApprovalId, Backlog, ChatConfig, ChatKind, Choice, ConversationId, DailyLimit,
    Destination, ImChannel, ImError, ImageRef, Inbound, Inlet, Kept, MemoryBacklog, MessageId, MessageRef, Post,
    Replacement, Replies, RunEnd, RunRecord, Said, Server, ServerConfig, ServerEvent, ServerSink, SessionKey,
    StatusStore, ThreadId, Trigger, Triggers, UserId,
};
use zx_testing::{EchoTool, WAIT, config, fast_retry, summarize, test_limits};

const PLATFORM: &str = "fake";
const CHAT: &str = "C1";
/// A direct chat with the asker.
const DIRECT: &str = "D1";
const ASKER: &str = "U-asker";
const APPROVER: &str = "U-approver";
const OTHER: &str = "U-other";

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
    /// A text the platform refuses to post; it records the post either way.
    refused_post: Option<String>,
    /// What every thread holds before the bot is mentioned there.
    history: Vec<Said>,
    /// Holds each reading of a thread's history, when there is one.
    history_gate: Option<Arc<Gate>>,
    /// What the chat itself holds before every mention posted there.
    chat: Vec<Said>,
    /// Users' names; the others have none.
    names: HashMap<String, String>,
    /// The images the platform keeps, by key, or why it cannot give one.
    images: HashMap<String, Result<Vec<u8>, String>>,
}

#[async_trait]
impl ImChannel for FakeChannel {
    async fn image(&self, _message: &MessageRef, image: &ImageRef, max_bytes: usize) -> Result<Vec<u8>, ImError> {
        match self.images.get(&image.key) {
            Some(Ok(bytes)) if bytes.len() > max_bytes => {
                Err(ImError::Transport(format!("the file is larger than {max_bytes} bytes")))
            }
            Some(Ok(bytes)) => Ok(bytes.clone()),
            Some(Err(why)) => Err(ImError::Transport(why.clone())),
            None => Err(ImError::Transport(format!("no image {}", image.key))),
        }
    }

    async fn thread_history(&self, _at: &Destination, _limit: usize) -> Result<Vec<Said>, ImError> {
        if let Some(gate) = &self.history_gate {
            gate.pass().await;
        }
        Ok(self.history.clone())
    }

    async fn chat_history(&self, _mention: &MessageRef, _limit: usize) -> Result<Vec<Said>, ImError> {
        Ok(self.chat.clone())
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
        if matches!((message, &self.refused_post), (Post::Text(text), Some(refused)) if text == refused) {
            return Err(ImError::Transport("post refused".into()));
        }
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
/// thread `slow-thread`, the reading of a thread's history, or a run that has an answer.
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

/// A summarizer that waits at its gate, then gives a summary citing the first record it read.
struct Held(Arc<Gate>);

#[async_trait]
impl Summarizer for Held {
    fn name(&self) -> &str {
        "held"
    }

    async fn summarize(&self, source: &SummarySource<'_>) -> Result<Summarized, String> {
        self.0.pass().await;
        Ok(Summarized {
            summary: CompactionSummary {
                facts: vec![SummaryFact {
                    text: "Old observation".into(),
                    source_entry_ids: vec![source.records[0].entry_id.clone()],
                }],
                inferences: Vec::new(),
                plans: Vec::new(),
            },
            usage: Usage::default(),
        })
    }
}

/// Holds a run at its gate once the model has finished an answer, before the run looks for
/// messages steered into it: a message sent meanwhile comes after the answer.
struct HeldAnswer(Arc<Gate>);

#[async_trait]
impl Hook for HeldAnswer {
    async fn on_event(&self, event: &Event, _ctx: &HookContext) -> HookResult<()> {
        if let EventKind::MessageEnd {
            message: Message::Assistant(answer),
            ..
        } = &event.event
            && answer.stop_reason == StopReason::Stop
        {
            self.0.pass().await;
        }
        Ok(())
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
    /// `approval_timeout`; everything else is allowed. A summary leaves the latest
    /// `keep_recent_bytes` of the talk as they are, the agent's default without a number,
    /// and with `summary_gate` is made by a summarizer that waits there, not by the model.
    /// With `answer_gate`, a run waits there each time the model has finished an answer.
    fn agent(
        self: &Arc<Self>,
        approval_timeout: Duration,
        keep_recent_bytes: Option<usize>,
        summary_gate: Option<Arc<Gate>>,
        answer_gate: Option<Arc<Gate>>,
    ) -> Agent {
        let tools: Vec<Arc<dyn Tool>> = vec![self.echo.clone()];
        let mut config = config(self.provider.clone(), tools);
        if let Some(bytes) = keep_recent_bytes {
            config.compaction.keep_recent_bytes = bytes;
        }
        if let Some(gate) = summary_gate {
            config.summarizer = Some(Arc::new(Held(gate)));
        }
        if let Some(gate) = answer_gate {
            config.hooks.push("held_answer", Arc::new(HeldAnswer(gate)));
        }
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

/// The server's events for the test: the records of runs apart, since every run has one.
struct Events {
    events: mpsc::UnboundedSender<ServerEvent>,
    ran: mpsc::UnboundedSender<RunRecord>,
}

impl ServerSink for Events {
    fn emit(&self, event: &ServerEvent) {
        // The test may have stopped reading.
        match event {
            ServerEvent::Ran(record) => self.ran.send(record.clone()).ok(),
            other => self.events.send(other.clone()).ok(),
        };
    }
}

/// A sink for a server whose events no test reads.
struct NullServerSink;

impl ServerSink for NullServerSink {
    fn emit(&self, _event: &ServerEvent) {}
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
    /// A text the channel refuses to post.
    refused_post: Option<String>,
    history: Vec<Said>,
    chat: Vec<Said>,
    names: HashMap<String, String>,
    /// The images the channel gives, by key.
    images: HashMap<String, Result<Vec<u8>, String>>,
    /// How much of the latest talk `/compact` leaves as it is; the agent's default without.
    keep_recent_bytes: Option<usize>,
    /// Where a summarizer of the host waits before it summarizes; the model summarizes
    /// without one.
    summary_gate: Option<Arc<Gate>>,
    /// Where a run waits once the model has finished an answer, before it looks for
    /// messages steered into it.
    answer_gate: Option<Arc<Gate>>,
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
            refused_post: None,
            history: Vec::new(),
            chat: Vec::new(),
            names: HashMap::new(),
            images: HashMap::new(),
            keep_recent_bytes: None,
            summary_gate: None,
            answer_gate: None,
        }
    }
}

struct Harness {
    inject: mpsc::Sender<Inbound>,
    sent: mpsc::UnboundedReceiver<Sent>,
    events: mpsc::UnboundedReceiver<ServerEvent>,
    /// The records of the runs that ended, apart from the other events.
    ran: mpsc::UnboundedReceiver<RunRecord>,
    cancel: CancellationToken,
    server: JoinHandle<Result<(), ImError>>,
    sessions: Arc<Sessions>,
    agent: Agent,
    replies: Replies,
    statuses: Option<Arc<Statuses>>,
    backlog: Arc<MemoryBacklog>,
    /// Where the test asks the server for conversations without a message.
    triggers: Triggers,
    /// Where the test replaces the chats served and the approvers.
    access: Access,
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
    let agent = sessions.agent(
        setup.approval_timeout,
        setup.keep_recent_bytes,
        setup.summary_gate,
        setup.answer_gate,
    );
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
        refused_post: setup.refused_post,
        history: setup.history,
        history_gate: setup.history_gate,
        chat: setup.chat,
        names: setup.names,
        images: setup.images,
    });
    let (events_to, events) = mpsc::unbounded_channel();
    let (ran_to, ran) = mpsc::unbounded_channel();
    let mut config = setup.config;
    if let Some(statuses) = &statuses {
        let store: Arc<dyn StatusStore> = statuses.clone();
        config.statuses = Some(store);
    }
    let backlog = setup.backlog;
    config.backlog = backlog.clone();
    let replies = config.replies.clone();
    let sink = Events {
        events: events_to,
        ran: ran_to,
    };
    let server = Server::new(config, agent.clone(), Arc::new(sink));
    let triggers = server.triggers();
    let access = server.access();
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    let server = tokio::spawn(async move { server.run(vec![channel], &stop).await });
    Harness {
        inject,
        sent,
        events,
        ran,
        cancel,
        server,
        sessions,
        agent,
        replies,
        statuses,
        backlog,
        triggers,
        access,
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
        images: Vec::new(),
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
            images: Vec::new(),
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

    /// The record of the next run that ends.
    async fn ran(&mut self) -> RunRecord {
        tokio::time::timeout(WAIT, self.ran.recv())
            .await
            .expect("no run ended in time")
            .expect("the sink is alive")
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
