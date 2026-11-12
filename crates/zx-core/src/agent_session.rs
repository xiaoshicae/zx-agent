//! Shared conversation API around the exclusively owned Runner.
//!
//! - `subscribers`: the subscribers of its events and the handle that keeps one attached
//!
//! A prompt or compaction takes ownership of the Runner, without a lock across await.
//! Its lease restores the Runner on completion, cancellation, panic or future drop.

mod subscribers;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::agent::{
    ActiveTools, CommandContext, CommandError, CommandOutcome, EventSink, LoopConfig, MessageQueues, QueuedMessage,
    SlashCommand,
};
use crate::ai::retry::RetryPolicy;
use crate::ai::{Provider, Reasoning};
use crate::compaction;
use crate::runner::{Limits, RunReport, Runner};
use crate::session::{CustomMessage, SessionName, SessionStore};
use crate::sync::lock;

use subscribers::EventHub;
pub use subscribers::Subscription;

/// Provider and model selected together, so a switch cannot expose a mixed pair.
pub struct ModelRef {
    /// Already configured by the host, including authentication and transport limits.
    pub provider: Arc<dyn Provider>,
    /// Non-empty model ID passed to that provider on subsequent prompts.
    pub model: String,
}

/// Session API errors. Operational failures remain in the run or compaction report.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AgentSessionError {
    /// A prompt, compaction or model change overlaps an active operation; it is never queued.
    #[error("session is running; queue input with queues().steer or queues().follow_up")]
    Busy,
    /// Model IDs must contain at least one non-whitespace character.
    #[error("model ID must not be empty")]
    InvalidModel,
    /// The entry a branch switch names is not in the session.
    #[error("no entry with that ID in the session")]
    EntryNotFound,
    /// The session could not record an entry; see [`SessionStore::writes_stopped`].
    #[error("cannot write the session: {0}")]
    WriteFailed(String),
}

enum State {
    // A full Runner is only needed while idle, so keep the state enum small.
    Idle(Box<Runner>),
    /// An operation holds the Runner; the token cancels it.
    Leased(CancellationToken),
}

/// One long-lived conversation, shared by its frontends through borrowed references or Arc.
/// State locks cover ownership transfers only; callbacks and asynchronous work run unlocked.
///
/// ```no_run
/// use std::sync::Arc;
/// use tokio_util::sync::CancellationToken;
/// use zx_core::AgentSession;
/// use zx_core::agent::Event;
/// use zx_core::agent_session::AgentSessionError;
/// use zx_core::runner::Runner;
///
/// # async fn example(runner: Runner) -> Result<(), AgentSessionError> {
/// let session = AgentSession::from(runner);
/// let _subscription = session.subscribe(Arc::new(|event: &Event| {
///     println!("{} #{} {}", event.run_id, event.seq, event.event.kind());
/// }));
/// let cancel = CancellationToken::new();
/// let first = session.prompt("Inspect the logs.", &cancel).await?;
/// let second = session.prompt("Summarize the findings.", &cancel).await?;
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

    /// Run a prompt against the retained conversation: the person's text, or a message from
    /// an extension (pi: `sendMessage` with `triggerTurn` while idle). Overlapping calls
    /// return Busy. The run uses a child of `cancel`: cancelling `cancel` cancels the run, but
    /// neither abort nor dropping the future cancels `cancel` or any later prompt. Dropping
    /// the future cancels its work and immediately releases the conversation.
    pub async fn prompt(
        &self,
        message: impl Into<QueuedMessage>,
        cancel: &CancellationToken,
    ) -> Result<RunReport, AgentSessionError> {
        // The conversion is host code: it runs before the conversation is leased.
        let message = message.into();
        let cancel = cancel.child_token();
        let mut lease = self.lease(cancel.clone())?;
        let report = lease.runner().run(message, &cancel).await;
        Ok(report)
    }

    /// Continue the conversation without new input; see [`Runner::continue_run`] for what is
    /// answered. An overlap returns Busy; `cancel` works as for `prompt`.
    pub async fn continue_run(&self, cancel: &CancellationToken) -> Result<RunReport, AgentSessionError> {
        let cancel = cancel.child_token();
        let mut lease = self.lease(cancel.clone())?;
        Ok(lease.runner().continue_run(&cancel).await)
    }

    /// Compact the retained conversation while holding the same exclusive lease as prompt.
    /// An overlap returns Busy; no eligible old prefix returns a no-op report. The summary
    /// uses a child of `cancel`: abort or future drop leaves `cancel` untouched, restores the
    /// Runner, and keeps an incomplete compaction out of context.
    pub async fn compact(&self, cancel: &CancellationToken) -> Result<compaction::Report, AgentSessionError> {
        let cancel = cancel.child_token();
        let mut lease = self.lease(cancel.clone())?;
        Ok(lease.runner().compact(&cancel).await)
    }

    /// The conversation's queues: to steer or follow up with the person's input (`steer`,
    /// `follow_up`) or a message from an extension (`steer_message`, `follow_up_message`), to
    /// record extension state during a run (`append_entry`), and to choose how many queued
    /// messages a request takes (`set_mode`).
    pub fn queues(&self) -> &MessageQueues {
        &self.queues
    }

    /// The tools the next request offers, shared with the running loop and the hooks (pi:
    /// `setActiveTools`). A change applies to the next request, also during a run.
    pub fn active_tools(&self) -> &ActiveTools {
        &self.active_tools
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
            self.session_id.clone(),
            self.queues.clone(),
            self.active_tools.clone(),
        );
        command.run(args, &ctx).await.map_err(CommandError::Failed)
    }

    /// Record state of an extension in the session while idle (pi: `appendEntry`); during
    /// a run, hooks and commands use `queues().append_entry` instead. Returns the entry's
    /// ID. A run returns Busy; a write failure returns WriteFailed.
    pub async fn append_entry(&self, custom_type: &str, data: Value) -> Result<String, AgentSessionError> {
        self.append_idle(async |store| store.append_custom_entry(custom_type, data).await)
            .await
    }

    /// Record a message from an extension while idle, without starting a run (pi:
    /// `sendMessage` while idle): the model sees it with the next prompt. Returns the
    /// entry's ID. A run returns Busy; a write failure returns WriteFailed.
    pub async fn append_custom_message(&self, message: CustomMessage) -> Result<String, AgentSessionError> {
        self.append_idle(async |store| store.append_custom_message(message).await)
            .await
    }

    /// Name the session while idle (pi: `setSessionName`): listings show the name instead of
    /// the first prompt, and the model never sees it. The latest name counts, whichever branch
    /// it was given on; `inspect` with [`SessionStore::session_name`] reads it. Returns the
    /// entry's ID. A run returns Busy; a write failure returns WriteFailed.
    pub async fn set_name(&self, name: SessionName) -> Result<String, AgentSessionError> {
        self.append_idle(async |store| store.append_session_name(name).await)
            .await
    }

    /// Write one record with `append` while idle, with the conversation leased as for a
    /// prompt. A run returns Busy; a write failure returns WriteFailed.
    async fn append_idle(
        &self,
        append: impl AsyncFnOnce(&mut SessionStore) -> anyhow::Result<String>,
    ) -> Result<String, AgentSessionError> {
        let mut lease = self.lease(CancellationToken::new())?;
        append(lease.runner().session_mut())
            .await
            .map_err(|error| AgentSessionError::WriteFailed(format!("{error:#}")))
    }

    /// Take the input that `queues().steer` and `follow_up` queued and no run consumed: steering
    /// messages first, then follow-ups, each oldest first. A run looks for input at the end
    /// of each turn, so input queued as it ends waits for the next run; a host that starts
    /// runs itself calls this when one ends and prompts with what it gets. Returns Busy
    /// during a prompt or compaction, whose loop may still take the input.
    pub fn take_queued(&self) -> Result<Vec<QueuedMessage>, AgentSessionError> {
        // Holding the state lock keeps a prompt from starting between the check and the take.
        self.idle(|_| self.queues.take_all())
    }

    /// Cancel only the active prompt or compaction. Returns false when idle. The short
    /// operations that read or write the session while idle (`inspect`, `append_entry`,
    /// `append_custom_message`, `set_name`) hold the conversation as well: during one of
    /// them this returns true, although they have nothing to cancel.
    pub fn abort(&self) -> bool {
        let cancel = match &*lock(&self.state) {
            State::Idle(_) => return false,
            State::Leased(cancel) => cancel.clone(),
        };
        // Waking a waiting future can call host-provided wakers; do it unlocked.
        cancel.cancel();
        true
    }

    /// Observe future events until the returned handle is dropped.
    /// Callbacks must return promptly. A panic detaches only that subscriber, the initial
    /// sink included; other subscribers continue.
    pub fn subscribe(&self, sink: Arc<dyn EventSink>) -> Subscription {
        self.events.subscribe(sink)
    }

    /// Switch provider and model while idle. Empty IDs return InvalidModel; a run returns
    /// Busy. History, tools, hooks, limits and exact-call approval grants are retained.
    /// No request is made and the session header keeps its original starting model.
    pub fn set_model(&self, model: ModelRef) -> Result<(), AgentSessionError> {
        if model.model.trim().is_empty() {
            return Err(AgentSessionError::InvalidModel);
        }
        let replaced = self.idle(|runner| runner.replace_model(model.provider, model.model))?;
        // A custom provider's destructor is host code too: the replaced provider is dropped
        // here, without the state lock, as the rejected one is above.
        drop(replaced);
        Ok(())
    }

    /// Change the reasoning level of later task requests while idle; `None` leaves the
    /// server's default. A run returns Busy. The change is recorded with the next request
    /// (`request_config`); no request is made now.
    pub fn set_reasoning(&self, reasoning: Option<Reasoning>) -> Result<(), AgentSessionError> {
        self.idle(|runner| {
            runner.replace_reasoning(reasoning);
        })
    }

    /// Move the conversation to another point of its session tree while idle (pi:
    /// `navigateTree`): the next prompt continues after `entry`, or before the first entry
    /// for `None`, on a new branch there. The entries of the branch left behind stay in the
    /// session, and a prompt after an answer whose tool calls have their results on that
    /// branch records those calls as not executed, as after a crash. Nothing is written
    /// until the next append, so a session resumed from its file continues where the file
    /// ends. A run returns Busy; an ID that is not in the session returns EntryNotFound,
    /// and the position does not change.
    pub fn branch(&self, entry: Option<&str>) -> Result<(), AgentSessionError> {
        self.idle(|runner| match entry {
            // The store's only error is an unknown ID, which the variant names.
            Some(id) => runner
                .session_mut()
                .branch(id)
                .map_err(|_| AgentSessionError::EntryNotFound),
            None => {
                runner.session_mut().reset_leaf();
                Ok(())
            }
        })?
    }

    /// Read the session while idle, for example to list its entries or find the current
    /// position. `f` runs with the conversation leased, as a prompt does: other calls get
    /// Busy until it returns, and no lock is held while host code runs. A run returns Busy.
    pub fn inspect<T>(&self, f: impl FnOnce(&SessionStore) -> T) -> Result<T, AgentSessionError> {
        let mut lease = self.lease(CancellationToken::new())?;
        Ok(f(lease.runner().session()))
    }

    /// Run `f` on the Runner under the state lock while idle; a run returns Busy. For quick
    /// changes of the kernel's own state only: `f` must not call host code, and what it
    /// returns is dropped after the lock is released, so it can hand back a host object,
    /// such as a replaced provider, to drop there. When busy, `f` is dropped unlocked too.
    fn idle<T>(&self, f: impl FnOnce(&mut Runner) -> T) -> Result<T, AgentSessionError> {
        let mut state = lock(&self.state);
        let State::Idle(runner) = &mut *state else {
            drop(state);
            drop(f);
            return Err(AgentSessionError::Busy);
        };
        Ok(f(runner))
    }

    /// Take the Runner for one operation that `cancel` cancels; Busy while another holds it.
    fn lease(&self, cancel: CancellationToken) -> Result<Lease<'_>, AgentSessionError> {
        let mut state = lock(&self.state);
        match std::mem::replace(&mut *state, State::Leased(cancel.clone())) {
            State::Idle(runner) => Ok(Lease {
                state: &self.state,
                runner: Some(runner),
                cancel,
            }),
            busy @ State::Leased(_) => {
                *state = busy;
                Err(AgentSessionError::Busy)
            }
        }
    }
}

impl From<Runner> for AgentSession {
    fn from(mut runner: Runner) -> Self {
        let config = runner.config();
        let queues = config.queues.clone();
        let active_tools = config.active_tools.clone();
        let commands = config.commands.clone();
        let cwd = config.cwd.clone();
        let session_id = runner.session().header().id.clone();
        let events = Arc::new(EventHub::default());
        let initial = runner.replace_sink(events.clone());
        // The original sink is permanent; only additional subscribers use RAII handles.
        events.attach_permanently(initial);
        Self {
            state: Mutex::new(State::Idle(Box::new(runner))),
            queues,
            active_tools,
            events,
            commands,
            cwd,
            session_id,
        }
    }
}

/// The lease of the Runner for one operation.
///
/// Run ownership is restored by Drop even when an awaiting caller disappears.
/// Option permits moving the Runner back exactly once; it is Some until Drop.
struct Lease<'a> {
    state: &'a Mutex<State>,
    runner: Option<Box<Runner>>,
    cancel: CancellationToken,
}

impl Lease<'_> {
    fn runner(&mut self) -> &mut Runner {
        self.runner
            .as_deref_mut()
            .expect("an active lease owns its Runner until Drop")
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        self.cancel.cancel();
        let runner = self
            .runner
            .take()
            .expect("an active lease returns its Runner exactly once");
        *lock(self.state) = State::Idle(runner);
    }
}
