//! Shared conversation API around the exclusively owned Runner.
//!
//! A prompt or compaction takes ownership of the Runner, without a lock across await.
//! Its lease restores the Runner on completion, cancellation, panic or future drop.

use std::fmt;
use std::num::NonZeroUsize;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

use tokio_util::sync::CancellationToken;

use crate::agent::{Event, EventSink, LoopConfig, MessageQueues};
use crate::ai::Provider;
use crate::ai::retry::RetryPolicy;
use crate::compaction;
use crate::runner::{Limits, RunReport, Runner};
use crate::session::SessionStore;
use crate::sync::lock;

/// Provider and model selected together, so a switch cannot expose a mixed pair.
pub struct ModelRef {
    /// Already configured by the host, including authentication and transport limits.
    pub provider: Arc<dyn Provider>,
    /// Non-empty model ID passed to that provider on subsequent prompts.
    pub model: String,
}

/// Session API errors. Operational failures remain in the run or compaction report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionError {
    /// A prompt, compaction or model change overlaps an active operation; it is never queued.
    Busy,
    /// Model IDs must contain at least one non-whitespace character.
    InvalidModel,
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => f.write_str("session is running; use steer or follow_up to submit input"),
            Self::InvalidModel => f.write_str("model ID must not be empty"),
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
/// use zx_agent::AgentSession;
/// use zx_agent::agent::Event;
/// use zx_agent::agent_session::SessionError;
/// use zx_agent::runner::Runner;
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
    // Queues and events are genuinely shared with the Runner while a prompt owns it.
    queues: Arc<MessageQueues>,
    events: Arc<EventHub>,
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
        let cancel = parent.child_token();
        let mut active = self.acquire(cancel.clone())?;
        let report = active.runner().run(text, &cancel).await;
        Ok(report)
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
        Subscription {
            events: Arc::downgrade(&self.events),
            listener: self.events.attach(sink),
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
        let queues = runner.config().queues.clone();
        let events = Arc::new(EventHub::default());
        let initial = runner.replace_sink(events.clone());
        // The original sink is permanent; only additional subscribers use RAII handles.
        drop(events.attach(initial));
        Self {
            state: Mutex::new(State::Idle(Box::new(runner))),
            queues,
            events,
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
/// dispatched snapshot may still finish. The handle does not keep the conversation alive.
#[must_use = "retain the subscription for as long as events are needed"]
pub struct Subscription {
    events: Weak<EventHub>,
    listener: Arc<Listener>,
}

impl Subscription {
    /// Whether a callback panic caused this subscriber to be detached.
    pub fn has_panicked(&self) -> bool {
        self.listener.failed.load(Ordering::SeqCst)
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(events) = self.events.upgrade() {
            lock(&events.listeners).retain(|listener| !Arc::ptr_eq(listener, &self.listener));
        }
    }
}

struct Listener {
    sink: Arc<dyn EventSink>,
    failed: AtomicBool,
}

#[derive(Default)]
struct EventHub {
    listeners: Mutex<Vec<Arc<Listener>>>,
    failures: AtomicUsize,
}

impl EventHub {
    fn attach(&self, sink: Arc<dyn EventSink>) -> Arc<Listener> {
        let listener = Arc::new(Listener {
            sink,
            failed: AtomicBool::new(false),
        });
        lock(&self.listeners).push(listener.clone());
        listener
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
    /// The next event; waits until one arrives. `None` once the session is gone.
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
