//! Shared conversation API around the exclusively owned Runner.
//!
//! A prompt or compaction takes ownership of the Runner, without a lock across await.
//! Its lease restores the Runner on completion, cancellation, panic or future drop.

use std::fmt;
use std::num::NonZeroUsize;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::agent::{
    ActiveTools, CommandContext, CommandError, CommandOutcome, Event, EventSink, LoopConfig, MessageQueues,
    QueuedMessage, SlashCommand,
};
use crate::ai::retry::RetryPolicy;
use crate::ai::{Provider, Reasoning};
use crate::compaction;
use crate::runner::{Limits, RunReport, Runner};
use crate::session::{CustomMessage, SessionStore, WritesStopped};
use crate::sync::lock;

/// Provider and model selected together, so a switch cannot expose a mixed pair.
pub struct ModelRef {
    /// Already configured by the host, including authentication and transport limits.
    pub provider: Arc<dyn Provider>,
    /// Non-empty model ID passed to that provider on subsequent prompts.
    pub model: String,
}

/// Session API errors. Operational failures remain in the run or compaction report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionError {
    /// A prompt, compaction or model change overlaps an active operation; it is never queued.
    Busy,
    /// Model IDs must contain at least one non-whitespace character.
    InvalidModel,
    /// The entry a branch switch names is not in the session.
    EntryNotFound,
    /// A tool name that no configured tool has.
    UnknownTool,
    /// The session could not record an entry; see [`AgentSession::writes_stopped`].
    WriteFailed(String),
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => f.write_str("session is running; use steer or follow_up to submit input"),
            Self::InvalidModel => f.write_str("model ID must not be empty"),
            Self::EntryNotFound => f.write_str("no entry with that ID in the session"),
            Self::UnknownTool => f.write_str("no configured tool has that name"),
            Self::WriteFailed(error) => write!(f, "cannot write the session: {error}"),
        }
    }
}

impl std::error::Error for SessionError {}

enum State {
    // A full Runner is only needed while idle, so keep the state enum small.
    Idle(Box<Runner>),
    Running(CancellationToken),
}

/// One long-lived conversation, shared by its frontends through borrowed references or Arc.
/// State locks cover ownership transfers only; callbacks and asynchronous work run unlocked.
///
/// ```no_run
/// use std::sync::Arc;
/// use zx_core::AgentSession;
/// use zx_core::agent::Event;
/// use zx_core::agent_session::SessionError;
/// use zx_core::runner::Runner;
///
/// # async fn example(runner: Runner) -> Result<(), SessionError> {
/// let session = AgentSession::from(runner);
/// let _subscription = session.subscribe(Arc::new(|event: &Event| {
///     println!("{} #{} {}", event.run_id, event.seq, event.event.kind());
/// }));
/// let first = session.prompt("Inspect the logs.").await?;
/// let second = session.prompt("Summarize the findings.").await?;
/// # Ok(())
/// # }
/// ```
pub struct AgentSession {
    state: Mutex<State>,
    // Queues, the tool set and events are genuinely shared with the Runner while a prompt
    // owns it.
    queues: Arc<MessageQueues>,
    active_tools: Arc<ActiveTools>,
    events: Arc<EventHub>,
    /// The configured tools' names, which the loop fixes when the Runner is built.
    tool_names: Vec<String>,
    commands: Vec<Arc<dyn SlashCommand>>,
    cwd: PathBuf,
    session_id: String,
}

impl AgentSession {
    /// Create a conversation. The initial sink stays attached for its lifetime.
    pub fn new(
        config: LoopConfig,
        retry: RetryPolicy,
        limits: Limits,
        session: SessionStore,
        sink: Arc<dyn EventSink>,
    ) -> Self {
        Self::from(Runner::new(config, retry, limits, session, sink))
    }

    /// Run a prompt against the retained conversation. Overlapping calls return Busy.
    /// Dropping the future cancels its work and immediately releases the conversation.
    pub async fn prompt(&self, text: impl Into<String>) -> Result<RunReport, SessionError> {
        self.prompt_with_cancel(text, &CancellationToken::new()).await
    }

    /// Like prompt, using a child of the caller's token. Neither abort nor future drop
    /// cancels that parent token or any subsequent prompt.
    pub async fn prompt_with_cancel(
        &self,
        text: impl Into<String>,
        parent: &CancellationToken,
    ) -> Result<RunReport, SessionError> {
        self.prompt_message_with_cancel(QueuedMessage::User(text.into()), parent)
            .await
    }

    /// Run with any message as the input: the person's text, or a message from an
    /// extension (pi: `sendMessage` with `triggerTurn` while idle). Otherwise like `prompt`.
    pub async fn prompt_message(&self, message: QueuedMessage) -> Result<RunReport, SessionError> {
        self.prompt_message_with_cancel(message, &CancellationToken::new())
            .await
    }

    /// Like prompt_message, using a child of the caller's token, as prompt_with_cancel does.
    pub async fn prompt_message_with_cancel(
        &self,
        message: QueuedMessage,
        parent: &CancellationToken,
    ) -> Result<RunReport, SessionError> {
        let cancel = parent.child_token();
        let mut active = self.acquire(cancel.clone())?;
        let report = active.runner().run_message(message, &cancel).await;
        Ok(report)
    }

    /// Continue the conversation without new input; see [`Runner::continue_run`] for what is
    /// answered. An overlap returns Busy.
    pub async fn continue_run(&self) -> Result<RunReport, SessionError> {
        self.continue_with_cancel(&CancellationToken::new()).await
    }

    /// Like continue_run, using a child of the caller's token, as prompt_with_cancel does.
    pub async fn continue_with_cancel(&self, parent: &CancellationToken) -> Result<RunReport, SessionError> {
        let cancel = parent.child_token();
        let mut active = self.acquire(cancel.clone())?;
        Ok(active.runner().continue_run(&cancel).await)
    }

    /// Compact the retained conversation while holding the same exclusive lease as prompt.
    /// An overlap returns Busy; no eligible old prefix returns a no-op report.
    pub async fn compact(&self) -> Result<compaction::Report, SessionError> {
        self.compact_with_cancel(&CancellationToken::new()).await
    }

    /// Like compact, using a child token. Abort or future drop leaves the parent token
    /// untouched, restores the Runner, and keeps incomplete summary attempts out of context.
    pub async fn compact_with_cancel(&self, parent: &CancellationToken) -> Result<compaction::Report, SessionError> {
        let cancel = parent.child_token();
        let mut active = self.acquire(cancel.clone())?;
        Ok(active.runner().compact(&cancel).await)
    }

    /// Queue input before the next model request, using the existing steering semantics.
    pub fn steer(&self, text: impl Into<String>) {
        self.queues.steer(text);
    }

    /// Queue input to be consumed when the current task would otherwise finish.
    pub fn follow_up(&self, text: impl Into<String>) {
        self.queues.follow_up(text);
    }

    /// The conversation's queues: to steer or follow up with a message from an extension
    /// (`steer_message`, `follow_up_message`), to record extension state during a run
    /// (`append_entry`), and to choose how many queued messages a request takes
    /// (`set_steering_mode`, `set_follow_up_mode`).
    pub fn queues(&self) -> &MessageQueues {
        &self.queues
    }

    /// The tools the next request offers, shared with the running loop and the hooks. A
    /// change applies to the next request, also during a run; `set_active_tools` checks the
    /// names first.
    pub fn active_tools(&self) -> &ActiveTools {
        &self.active_tools
    }

    /// Offer the model only `names` from the next request on (pi: `setActiveTools`), also
    /// during a run. A name that no configured tool has returns UnknownTool and changes
    /// nothing. An empty list offers no tool.
    pub fn set_active_tools(&self, names: &[&str]) -> Result<(), SessionError> {
        if names
            .iter()
            .any(|name| !self.tool_names.iter().any(|known| known == name))
        {
            return Err(SessionError::UnknownTool);
        }
        self.active_tools.set(names.iter().copied());
        Ok(())
    }

    /// The slash commands extensions registered, in registration order.
    pub fn commands(&self) -> &[Arc<dyn SlashCommand>] {
        &self.commands
    }

    /// Run the registered command called `name` with `args`, also while a run is in
    /// progress (pi: extension commands run at once). The caller shows the outcome's lines
    /// and sends its `prompt`, if any, as it would the person's input.
    pub async fn run_command(&self, name: &str, args: &str) -> Result<CommandOutcome, CommandError> {
        let command = self
            .commands
            .iter()
            .find(|command| command.name() == name)
            .ok_or(CommandError::Unknown)?;
        let ctx = CommandContext::new(
            self.cwd.clone(),
            Some(self.session_id.clone()),
            self.queues.clone(),
            self.active_tools.clone(),
        );
        command.run(args, &ctx).await.map_err(CommandError::Failed)
    }

    /// Record state of an extension in the session while idle (pi: `appendEntry`); during
    /// a run, hooks and commands use `queues().append_entry` instead. Returns the entry's
    /// ID. A run returns Busy; a write failure returns WriteFailed.
    pub async fn append_entry(&self, custom_type: &str, data: Value) -> Result<String, SessionError> {
        let mut active = self.acquire(CancellationToken::new())?;
        active
            .runner()
            .session_mut()
            .append_custom_entry(custom_type, data)
            .await
            .map_err(|error| SessionError::WriteFailed(format!("{error:#}")))
    }

    /// Record a message from an extension while idle, without starting a run (pi:
    /// `sendMessage` while idle): the model sees it with the next prompt. Returns the
    /// entry's ID. A run returns Busy; a write failure returns WriteFailed.
    pub async fn append_custom_message(&self, message: CustomMessage) -> Result<String, SessionError> {
        let mut active = self.acquire(CancellationToken::new())?;
        active
            .runner()
            .session_mut()
            .append_custom_message(message)
            .await
            .map_err(|error| SessionError::WriteFailed(format!("{error:#}")))
    }

    /// Take the input that `steer` and `follow_up` queued and no run consumed: steering
    /// messages first, then follow-ups, each oldest first. A run looks for input at the end
    /// of each turn, so input queued as it ends waits for the next run; a host that starts
    /// runs itself calls this when one ends and prompts with what it gets. Returns Busy
    /// during a prompt or compaction, whose loop may still take the input.
    pub fn take_queued(&self) -> Result<Vec<QueuedMessage>, SessionError> {
        // Holding the state lock keeps a prompt from starting between the check and the take.
        let state = lock(&self.state);
        match &*state {
            State::Running(_) => Err(SessionError::Busy),
            State::Idle(_) => Ok(self.queues.take_all()),
        }
    }

    /// Cancel only the active prompt or compaction. Returns false when idle.
    pub fn abort(&self) -> bool {
        let cancel = match &*lock(&self.state) {
            State::Idle(_) => return false,
            State::Running(cancel) => cancel.clone(),
        };
        // Waking a waiting future can call host-provided wakers; do it unlocked.
        cancel.cancel();
        true
    }

    /// Observe future events until the returned handle is dropped.
    /// Callbacks must return promptly. A panic detaches only that subscriber, and is
    /// observable through the handle and failed_subscribers; other subscribers continue.
    pub fn subscribe(&self, sink: Arc<dyn EventSink>) -> Subscription {
        let (listener, failed) = self.events.attach(sink);
        Subscription {
            events: Arc::downgrade(&self.events),
            listener,
            failed,
        }
    }

    /// Receive future events through a channel that holds at most `capacity` of them, for
    /// exporters that must never slow a run down. While the channel is full, new events are
    /// dropped and counted, which also leaves a gap in their `seq`. Dropping the stream
    /// unsubscribes.
    pub fn subscribe_bounded(&self, capacity: NonZeroUsize) -> EventStream {
        let (sender, receiver) = tokio::sync::mpsc::channel(capacity.get());
        let dropped = Arc::new(AtomicU64::new(0));
        let sink = Arc::new(BoundedSink {
            sender,
            dropped: dropped.clone(),
        });
        EventStream {
            receiver,
            dropped,
            _subscription: self.subscribe(sink),
        }
    }

    /// Number of subscribers detached because their callback panicked, initial sink included.
    pub fn failed_subscribers(&self) -> usize {
        self.events.failures.load(Ordering::SeqCst)
    }

    /// Why the session no longer writes, or `None` while it does; see [`WritesStopped`]. A
    /// session that stopped writing fails every later prompt, so a host that keeps sessions
    /// open checks this after a failed run. Busy during a prompt or compaction.
    pub fn writes_stopped(&self) -> Result<Option<WritesStopped>, SessionError> {
        match &*lock(&self.state) {
            State::Idle(runner) => Ok(runner.session().writes_stopped().cloned()),
            State::Running(_) => Err(SessionError::Busy),
        }
    }

    /// Revoke all session approvals while idle. A concurrent run returns Busy.
    pub fn clear_approvals(&self) -> Result<(), SessionError> {
        match &mut *lock(&self.state) {
            State::Idle(runner) => {
                runner.clear_approvals();
                Ok(())
            }
            State::Running(_) => Err(SessionError::Busy),
        }
    }

    /// Switch provider and model while idle. Empty IDs return InvalidModel; a run returns
    /// Busy. History, tools, hooks, limits and exact-call approval grants are retained.
    /// No request is made and the session header keeps its original starting model.
    pub fn set_model(&self, model: ModelRef) -> Result<(), SessionError> {
        let previous = {
            let mut state = lock(&self.state);
            match &mut *state {
                State::Running(_) => return Err(SessionError::Busy),
                State::Idle(runner) => {
                    if model.model.trim().is_empty() {
                        return Err(SessionError::InvalidModel);
                    }
                    runner.replace_model(model.provider, model.model)
                }
            }
        };
        // A custom provider's destructor is host code too; drop it without the state lock.
        drop(previous);
        Ok(())
    }

    /// Change the reasoning level of later task requests while idle; `None` leaves the
    /// server's default. A run returns Busy. The change is recorded with the next request
    /// (`request_config`); no request is made now.
    pub fn set_reasoning(&self, reasoning: Option<Reasoning>) -> Result<(), SessionError> {
        let mut state = lock(&self.state);
        match &mut *state {
            State::Running(_) => Err(SessionError::Busy),
            State::Idle(runner) => {
                runner.replace_reasoning(reasoning);
                Ok(())
            }
        }
    }

    /// Move the conversation to another point of its session tree while idle (pi:
    /// `navigateTree`): the next prompt continues after `entry`, or before the first entry
    /// for `None`, on a new branch there. The entries of the branch left behind stay in the
    /// session, and a prompt after an answer whose tool calls have their results on that
    /// branch records those calls as not executed, as after a crash. Nothing is written
    /// until the next append, so a session resumed from its file continues where the file
    /// ends. A run returns Busy; an ID that is not in the session returns EntryNotFound,
    /// and the position does not change.
    pub fn branch(&self, entry: Option<&str>) -> Result<(), SessionError> {
        let mut state = lock(&self.state);
        match &mut *state {
            State::Running(_) => Err(SessionError::Busy),
            State::Idle(runner) => match entry {
                // The store's only error is an unknown ID, which the variant names.
                Some(id) => runner.session_mut().branch(id).map_err(|_| SessionError::EntryNotFound),
                None => {
                    runner.session_mut().reset_leaf();
                    Ok(())
                }
            },
        }
    }

    /// Read the session while idle, for example to list its entries or find the current
    /// position. `f` runs with the conversation leased, as a prompt does: other calls get
    /// Busy until it returns, and no lock is held while host code runs. A run returns Busy.
    pub fn inspect<T>(&self, f: impl FnOnce(&SessionStore) -> T) -> Result<T, SessionError> {
        let mut active = self.acquire(CancellationToken::new())?;
        Ok(f(active.runner().session()))
    }

    fn acquire(&self, cancel: CancellationToken) -> Result<ActiveRun<'_>, SessionError> {
        let mut state = lock(&self.state);
        match std::mem::replace(&mut *state, State::Running(cancel.clone())) {
            State::Idle(runner) => Ok(ActiveRun {
                state: &self.state,
                runner: Some(runner),
                cancel,
            }),
            busy @ State::Running(_) => {
                *state = busy;
                Err(SessionError::Busy)
            }
        }
    }
}

impl From<Runner> for AgentSession {
    fn from(mut runner: Runner) -> Self {
        let config = runner.config();
        let queues = config.queues.clone();
        let active_tools = config.active_tools.clone();
        let tool_names = config.spec.tools.iter().map(|tool| tool.name().to_string()).collect();
        let commands = config.commands.clone();
        let cwd = config.spec.cwd.clone();
        let session_id = runner.session().header().id.clone();
        let events = Arc::new(EventHub::default());
        let initial = runner.replace_sink(events.clone());
        // The original sink is permanent; only additional subscribers use RAII handles.
        drop(events.attach(initial));
        Self {
            state: Mutex::new(State::Idle(Box::new(runner))),
            queues,
            active_tools,
            events,
            tool_names,
            commands,
            cwd,
            session_id,
        }
    }
}

/// Run ownership is restored by Drop even when an awaiting caller disappears.
/// Option permits moving the Runner back exactly once; it is Some until Drop.
struct ActiveRun<'a> {
    state: &'a Mutex<State>,
    runner: Option<Box<Runner>>,
    cancel: CancellationToken,
}

impl ActiveRun<'_> {
    fn runner(&mut self) -> &mut Runner {
        self.runner
            .as_deref_mut()
            .expect("an active lease owns its Runner until Drop")
    }
}

impl Drop for ActiveRun<'_> {
    fn drop(&mut self) {
        self.cancel.cancel();
        let runner = self
            .runner
            .take()
            .expect("an active lease returns its Runner exactly once");
        *lock(self.state) = State::Idle(runner);
    }
}

/// Keeps an additional event subscriber attached. Dropping it unsubscribes; an already
/// dispatched snapshot may still finish. The handle keeps neither the conversation nor the
/// sink alive: the session owns its subscribers and releases them when it is dropped.
#[must_use = "retain the subscription for as long as events are needed"]
pub struct Subscription {
    events: Weak<EventHub>,
    /// Identifies the subscriber; the session holds the only strong reference.
    listener: Weak<Listener>,
    failed: Arc<AtomicBool>,
}

impl Subscription {
    /// Whether a callback panic caused this subscriber to be detached.
    pub fn has_panicked(&self) -> bool {
        self.failed.load(Ordering::SeqCst)
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(events) = self.events.upgrade() {
            // The weak reference keeps the allocation, so no other listener can share its address.
            lock(&events.listeners).retain(|listener| !std::ptr::eq(Arc::as_ptr(listener), self.listener.as_ptr()));
        }
    }
}

struct Listener {
    sink: Arc<dyn EventSink>,
    failed: Arc<AtomicBool>,
}

#[derive(Default)]
struct EventHub {
    listeners: Mutex<Vec<Arc<Listener>>>,
    failures: AtomicUsize,
}

impl EventHub {
    fn attach(&self, sink: Arc<dyn EventSink>) -> (Weak<Listener>, Arc<AtomicBool>) {
        let failed = Arc::new(AtomicBool::new(false));
        let listener = Arc::new(Listener {
            sink,
            failed: failed.clone(),
        });
        let handle = Arc::downgrade(&listener);
        lock(&self.listeners).push(listener);
        (handle, failed)
    }
}

/// Events of an [`AgentSession`] through a bounded channel; see
/// [`AgentSession::subscribe_bounded`].
pub struct EventStream {
    receiver: tokio::sync::mpsc::Receiver<Event>,
    // Shared with the sink that counts: an atomic counter, read here and written there.
    dropped: Arc<AtomicU64>,
    _subscription: Subscription,
}

impl EventStream {
    /// The next event; waits until one arrives. `None` once the session is gone and the
    /// events it sent before are read.
    pub async fn recv(&mut self) -> Option<Event> {
        self.receiver.recv().await
    }

    /// The next event if one is waiting, without waiting.
    pub fn try_recv(&mut self) -> Option<Event> {
        self.receiver.try_recv().ok()
    }

    /// Events dropped so far because the channel was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::SeqCst)
    }
}

struct BoundedSink {
    sender: tokio::sync::mpsc::Sender<Event>,
    dropped: Arc<AtomicU64>,
}

impl EventSink for BoundedSink {
    fn emit(&self, event: &Event) {
        match self.sender.try_send(event.clone()) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::SeqCst);
            }
            // The stream is gone; its subscription is being dropped as well.
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
        }
    }
}

impl EventSink for EventHub {
    fn emit(&self, event: &Event) {
        // Snapshot only the Arc handles. Reentrant subscribe, unsubscribe and abort must
        // never run under this lock.
        let listeners = lock(&self.listeners).clone();
        let mut failed = false;
        for listener in &listeners {
            if listener.failed.load(Ordering::SeqCst) {
                continue;
            }
            if catch_unwind(AssertUnwindSafe(|| listener.sink.emit(event))).is_err() {
                if !listener.failed.swap(true, Ordering::SeqCst) {
                    self.failures.fetch_add(1, Ordering::SeqCst);
                }
                failed = true;
            }
        }
        if failed {
            // Snapshot references keep removed sinks alive until after unlocking, so even
            // a custom destructor cannot run while the subscriber list is locked.
            lock(&self.listeners).retain(|listener| !listener.failed.load(Ordering::SeqCst));
        }
    }
}
