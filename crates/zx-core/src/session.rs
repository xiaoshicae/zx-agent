//! A conversation, shared by its frontends, and its runs. An [`Agent`](crate::Agent) makes
//! the sessions.
//!
//! - `run`: [`Run`], one prompt, continuation or compaction in progress: its events and its
//!   result
//!
//! A run takes ownership of the Runner, without a lock across await. Its lease restores the
//! Runner when the run ends or is dropped, also by a panic.

mod run;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use self::run::EVENT_BUFFER;
pub use self::run::Run;
use crate::ai::retry::RetryPolicy;
use crate::ai::{Provider, Reasoning};
use crate::compaction;
use crate::events::Event;
use crate::extensions::{CommandContext, CommandOutcome, SlashCommand};
use crate::input::{InputQueues, PendingInput};
use crate::policy::{ApprovalDecision, ApprovalGate};
use crate::runner::{LoopConfig, RunLimits, RunLink, Runner, Start};
use crate::sync::lock;
use crate::tools::ActiveTools;
use crate::transcript::{CustomMessage, SessionName, Transcript};

/// What a new session starts with.
#[derive(Clone, Debug)]
pub struct SessionOptions {
    /// Where the session's tools and hooks work, now and whenever it is opened again: the
    /// session's header records it, and a listing can ask for the sessions of a directory.
    /// A relative path is taken from the process's current directory when the session is
    /// created.
    pub cwd: PathBuf,
    /// What the host says about the session, such as its tenant or the chat it belongs to.
    /// The header keeps them, and a listing can ask for the sessions with given labels.
    pub labels: BTreeMap<String, String>,
}

impl SessionOptions {
    /// A session that works in `cwd`, without labels.
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            labels: BTreeMap::new(),
        }
    }
}

/// Session API errors. Operational failures remain in the run or compaction report.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    /// A run, compaction or model change overlaps an active operation; it is never queued.
    #[error("session is running; queue input with queues().steer or queues().follow_up")]
    Busy,
    /// Model IDs must contain at least one non-whitespace character.
    #[error("model ID must not be empty")]
    InvalidModel,
    /// The entry a branch switch names is not in the session.
    #[error("no entry with that ID in the session")]
    EntryNotFound,
    /// The session could not record an entry. When its storage failed, the session closes
    /// itself once this operation ends, and the agent opens it again from its storage; an
    /// entry it refuses before writing, such as a custom record without a custom type, leaves
    /// it open.
    #[error("cannot write the session: {0}")]
    WriteFailed(String),
    /// No approval request with that ID waits for an answer: it was answered already, nobody
    /// answered it in time, or its run was cancelled.
    #[error("no approval request with that ID is waiting")]
    UnknownApproval,
    /// The session is closed: the agent closed it
    /// ([`Agent::close_session`](crate::Agent::close_session)), or it closed itself because
    /// its record could no longer be written. Its record stays in the storage, where the
    /// agent opens it again ([`Agent::open_session`](crate::Agent::open_session)).
    #[error("session is closed; the agent opens it again from its storage")]
    Closed,
}

/// Why [`Session::run_command`] returned no outcome.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CommandError {
    /// No extension registered a command with that name.
    #[error("unknown command")]
    Unknown,
    /// The command ran and failed with this message.
    #[error("command failed: {0}")]
    Failed(String),
    /// The session is closed, as [`SessionError::Closed`] says.
    #[error("{}", SessionError::Closed)]
    Closed,
}

enum State {
    // A full Runner is only needed while idle, so keep the state enum small.
    Idle(Box<Runner>),
    /// An operation holds the Runner: a run or a compaction, which the token cancels, or one
    /// of the short operations that read or write the session while idle, which has none.
    Leased(Option<CancellationToken>),
    /// Closed while an operation held the Runner: the operation was cancelled, and its lease
    /// drops the Runner instead of returning it.
    Closing,
    /// The Runner is gone, and with it the transcript, the tools and the hooks.
    Closed,
}

impl State {
    /// Why an operation cannot take the Runner in this state, which is not `Idle`: another
    /// operation holds it, or the session is closed.
    fn refusal(&self) -> SessionError {
        if self.is_closed() {
            SessionError::Closed
        } else {
            SessionError::Busy
        }
    }

    /// Whether the session is closed, or closes once the operation that holds it ends.
    fn is_closed(&self) -> bool {
        matches!(self, Self::Closing | Self::Closed)
    }
}

/// One long-lived conversation, made by an [`Agent`](crate::Agent). A handle: its clones
/// are the same conversation, so that a frontend can read a run in one task and cancel or
/// steer it from another. State locks cover ownership transfers only; host code and
/// asynchronous work run unlocked. Once the session is closed, every operation that needs the
/// conversation returns [`SessionError::Closed`].
///
/// A session whose record can no longer be written closes itself when the operation that
/// met it ends: a write failed or timed out, or its caller dropped the run while a record
/// was on its way, so that nobody knows whether the storage has it. The storage holds what
/// was recorded, and the agent opens the session again from there.
///
/// ```no_run
/// use zx_core::session::{Session, SessionError};
///
/// # async fn example(session: Session) -> Result<(), SessionError> {
/// let first = session.send("Inspect the logs.")?.wait().await;
/// let second = session.send("Summarize the findings.")?.wait().await;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct Session {
    shared: Arc<Shared>,
}

struct Shared {
    state: Mutex<State>,
    // The queues and the tool set are genuinely shared with the Runner while a run owns it.
    queues: Arc<InputQueues>,
    active_tools: Arc<ActiveTools>,
    approvals: Arc<ApprovalGate>,
    commands: Vec<Arc<dyn SlashCommand>>,
    tool_names: Vec<String>,
    cwd: PathBuf,
    id: String,
}

impl Session {
    /// A conversation recorded in `transcript`, running with `config` and the `compaction`
    /// settings that the agent checked when it was made.
    pub(crate) fn new(
        config: LoopConfig,
        retry: RetryPolicy,
        limits: RunLimits,
        compaction: compaction::Config,
        transcript: Transcript,
    ) -> Self {
        let runner = Runner::new(config, retry, limits, transcript).with_compaction(compaction);
        let config = runner.config();
        Self {
            shared: Arc::new(Shared {
                queues: config.queues.clone(),
                active_tools: config.active_tools.clone(),
                approvals: runner.approvals().clone(),
                commands: config.commands.clone(),
                tool_names: config.tools.names().map(str::to_string).collect(),
                cwd: config.cwd.clone(),
                id: runner.transcript().header().id.clone(),
                state: Mutex::new(State::Idle(Box::new(runner))),
            }),
        }
    }

    /// The session's ID, as its header records it.
    pub fn id(&self) -> &str {
        &self.shared.id
    }

    /// The directory the session's tools and hooks work in: the one it was created in.
    pub fn cwd(&self) -> &Path {
        &self.shared.cwd
    }

    /// Start a run with `input`: the person's text, or a message from an extension (pi:
    /// `sendMessage` with `triggerTurn` while idle), with a hold of the host's if it has one
    /// ([`PendingInput::held`]), which ends once the session has recorded the input, or with a
    /// run that ends without recording it. This only takes the conversation; the returned
    /// [`Run`] makes the requests as it is read. An overlap returns Busy.
    ///
    /// A run cancelled while the input hooks ([`Hook::on_input`](crate::hooks::Hook::on_input))
    /// run records no input: recorded without their rewrite, it would reach the model as it
    /// was written. Without input hooks, the input is recorded before the run notices the
    /// cancellation.
    pub fn send(&self, input: impl Into<PendingInput>) -> Result<Run, SessionError> {
        // The conversion is host code: it runs before the conversation is leased.
        self.start(Start::Message(input.into()))
    }

    /// Start a run without new input: the model answers the conversation as it is. Whether
    /// there is something to answer is decided once interrupted calls are recovered:
    ///
    /// - the conversation ends with input or a tool result: the model answers it, for
    ///   example after a crash left results to recover;
    /// - it ends with an answer that failed or was cancelled, whose calls never ran: that
    ///   answer is omitted with a `context_edit`, as a retry does, and the model answers again;
    /// - it ends with a finished answer: queued steering, or else follow-up, input becomes the
    ///   next input; with none queued the run ends with an error and makes no request, as it
    ///   does for an empty conversation.
    ///
    /// An overlap returns Busy.
    pub fn continue_run(&self) -> Result<Run, SessionError> {
        self.start(Start::Continue)
    }

    fn start(&self, from: Start) -> Result<Run, SessionError> {
        let (mut lease, link, events) = self.begin()?;
        let id = link.id.clone();
        Ok(Run::new(
            id,
            events,
            async move { lease.runner().run(from, &link).await },
        ))
    }

    /// Summarize the old prefix of the conversation without executing a task or tools, as a
    /// run of its own, with the same exclusive lease of the conversation as a prompt. An
    /// overlap returns Busy; no eligible old prefix gives a no-op report. A cancelled or
    /// dropped run keeps an incomplete compaction out of the context.
    pub fn compact(&self) -> Result<Run<compaction::Report>, SessionError> {
        let (mut lease, link, events) = self.begin()?;
        let id = link.id.clone();
        Ok(Run::new(id, events, async move { lease.runner().compact(&link).await }))
    }

    /// Take the conversation for a run: its lease, what links the run to its reader, and
    /// the reader's end of its events.
    fn begin(&self) -> Result<(Lease, RunLink, mpsc::Receiver<Event>), SessionError> {
        let cancel = CancellationToken::new();
        let lease = self.lease(Some(cancel.clone()))?;
        let (events, reader) = mpsc::channel(EVENT_BUFFER);
        let link = RunLink {
            id: uuid::Uuid::new_v4().to_string(),
            cancel,
            events,
        };
        Ok((lease, link, reader))
    }

    /// The conversation's queues: to steer or follow up with the person's input or a message
    /// from an extension (`steer`, `follow_up`), to record extension state during a run
    /// (`append_custom_entry`), and to choose how many queued
    /// messages a request takes (`set_mode`). A clone is a handle to the same queues, for a
    /// part of a frontend that only feeds them.
    pub fn queues(&self) -> &Arc<InputQueues> {
        &self.shared.queues
    }

    /// The names of the session's tools, in the order requests declare them;
    /// [`active_tools`](Self::active_tools) chooses which of them a request offers.
    pub fn tool_names(&self) -> &[String] {
        &self.shared.tool_names
    }

    /// The tools the next request offers, shared with the running loop and the hooks (pi:
    /// `setActiveTools`). A change applies to the next request, also during a run.
    pub fn active_tools(&self) -> &ActiveTools {
        &self.shared.active_tools
    }

    /// The slash commands extensions registered, in registration order.
    pub fn commands(&self) -> &[Arc<dyn SlashCommand>] {
        &self.shared.commands
    }

    /// Run the registered command called `name` with `args`, also while a run is in
    /// progress (pi: extension commands run at once). The caller shows the outcome's lines
    /// and sends its `prompt`, if any, as it would the person's input.
    pub async fn run_command(&self, name: &str, args: &str) -> Result<CommandOutcome, CommandError> {
        if self.is_closed() {
            return Err(CommandError::Closed);
        }
        let shared = &self.shared;
        let command = shared
            .commands
            .iter()
            .find(|command| command.name() == name)
            .ok_or(CommandError::Unknown)?;
        let ctx = CommandContext::new(
            shared.id.clone(),
            shared.cwd.clone(),
            shared.queues.clone(),
            shared.active_tools.clone(),
        );
        command.run(args, &ctx).await.map_err(CommandError::Failed)
    }

    /// Record state of an extension in the session while idle (pi: `appendEntry`); during
    /// a run, hooks and commands use `queues().append_custom_entry` instead. Returns the entry's
    /// ID. A run returns Busy; a write failure returns WriteFailed.
    pub async fn append_custom_entry(&self, custom_type: &str, data: Value) -> Result<String, SessionError> {
        self.append_idle(async |runner| runner.transcript_mut().append_custom_entry(custom_type, data).await)
            .await
    }

    /// Record a message from an extension while idle, without starting a run (pi:
    /// `sendMessage` while idle): the model sees it with the next prompt. Calls that an
    /// interrupted run left without a result get theirs first, as a run's start gives them,
    /// so that the message does not come between the calls and their results. Returns the
    /// entry's ID. A run returns Busy; a write failure returns WriteFailed.
    pub async fn append_custom_message(&self, message: CustomMessage) -> Result<String, SessionError> {
        self.append_idle(async |runner| {
            runner.recover_interrupted_calls().await?;
            runner.transcript_mut().append_custom_message(message).await
        })
        .await
    }

    /// Name the session while idle (pi: `setSessionName`): listings show the name instead of
    /// the first prompt, and the model never sees it. The latest name counts, whichever branch
    /// it was given on; `with_transcript` with [`Transcript::session_name`] reads it. Returns
    /// the entry's ID. A run returns Busy; a write failure returns WriteFailed.
    pub async fn set_name(&self, name: SessionName) -> Result<String, SessionError> {
        self.append_idle(async |runner| runner.transcript_mut().append_session_name(name).await)
            .await
    }

    /// Write with `append` while idle, with the conversation leased as for a run; returns
    /// the ID of the record it writes. A run returns Busy; a write failure returns
    /// WriteFailed.
    async fn append_idle(
        &self,
        append: impl AsyncFnOnce(&mut Runner) -> anyhow::Result<String>,
    ) -> Result<String, SessionError> {
        let mut lease = self.lease(None)?;
        append(lease.runner())
            .await
            .map_err(|error| SessionError::WriteFailed(format!("{error:#}")))
    }

    /// Take the input that `queues().steer` and `follow_up` queued and no run consumed, each
    /// with its hold: steering messages first, then follow-ups, each oldest first. A run looks
    /// for input at the end of each step, so input queued as it ends waits for the next run;
    /// a host that starts runs itself calls this when one ends and sends what it gets
    /// ([`PendingInput::into_input`]). Returns Busy during a run or compaction, whose loop
    /// may still take the input.
    pub fn take_queued(&self) -> Result<Vec<PendingInput>, SessionError> {
        // Holding the state lock keeps a run from starting between the check and the take;
        // the holds are handed out, not dropped, under it.
        self.idle(|_| self.shared.queues.take_all())
    }

    /// Cancel the run or compaction in progress; its [`Run`] then ends with a cancelled
    /// result. Returns false when there is none: idle, closed, or during one of the short
    /// operations that read or write the session while idle (`with_transcript`,
    /// `append_custom_entry`, `append_custom_message`, `set_name`).
    pub fn cancel(&self) -> bool {
        let cancel = match &*lock(&self.shared.state) {
            State::Leased(Some(cancel)) => cancel.clone(),
            State::Idle(_) | State::Leased(None) | State::Closing | State::Closed => return false,
        };
        // Waking a waiting future can call host-provided wakers; do it unlocked.
        cancel.cancel();
        true
    }

    /// Whether this session is closed, or closes as soon as its cancelled operation ends: the
    /// agent closed it, or it closed itself because its record could no longer be written.
    /// A closed handle stays closed; [`Agent::open_session`](crate::Agent::open_session)
    /// gives the session again.
    pub fn is_closed(&self) -> bool {
        lock(&self.shared.state).is_closed()
    }

    /// Close the conversation: cancel the operation in progress, if there is one, and let go
    /// of the transcript, the tools and the hooks, at once when idle, else when the cancelled
    /// operation has ended. Returns false when it was closed already.
    pub(crate) fn close(&self) -> bool {
        let previous = {
            let mut state = lock(&self.shared.state);
            let next = match &*state {
                State::Idle(_) => State::Closed,
                State::Leased(_) => State::Closing,
                State::Closing | State::Closed => return false,
            };
            std::mem::replace(&mut *state, next)
        };
        match previous {
            State::Leased(Some(cancel)) => cancel.cancel(),
            // The Runner is dropped here, without the state lock: closing a storage and
            // dropping the tools and hooks is host code.
            State::Idle(_) | State::Leased(None) | State::Closing | State::Closed => {}
        }
        true
    }

    /// Answer the approval request `approval_id`, which an `approval_requested` event of the
    /// run in progress announced: allow the call once, allow identical calls for the rest
    /// of the session, or deny it with a reason the model hears. The run goes on at once.
    /// UnknownApproval when no such request waits any more, so a late answer never reaches
    /// another call.
    pub fn resolve_approval(&self, approval_id: &str, decision: ApprovalDecision) -> Result<(), SessionError> {
        if self.shared.approvals.resolve(approval_id, decision) {
            Ok(())
        } else {
            Err(SessionError::UnknownApproval)
        }
    }

    /// Switch provider and model together while idle, so that no request sees a mixed pair:
    /// `provider` as the host configured it, authentication and transport limits included,
    /// and `model`, the ID later prompts pass to it. An empty ID returns InvalidModel; a run
    /// returns Busy. History, tools, hooks, limits and exact-call approval grants are
    /// kept. No request is made and the session header keeps its original starting model.
    pub fn set_model(&self, provider: Arc<dyn Provider>, model: impl Into<String>) -> Result<(), SessionError> {
        let model = model.into();
        if model.trim().is_empty() {
            return Err(SessionError::InvalidModel);
        }
        let replaced = self.idle(|runner| runner.replace_model(provider, model))?;
        // A custom provider's destructor is host code too: the replaced provider is dropped
        // here, without the state lock, as the rejected one is above.
        drop(replaced);
        Ok(())
    }

    /// Change the reasoning level of later task requests while idle; `None` leaves the
    /// server's default. A run returns Busy. The change is recorded with the next request
    /// (`request_config`); no request is made now.
    pub fn set_reasoning(&self, reasoning: Option<Reasoning>) -> Result<(), SessionError> {
        self.idle(|runner| runner.set_reasoning(reasoning))
    }

    /// Move the conversation to another point of its session tree while idle (pi:
    /// `navigateTree`): the next run continues after `entry`, or before the first entry
    /// for `None`, on a new branch there. The entries of the branch left behind stay in the
    /// session, and a run after an answer whose tool calls have their results on that
    /// branch records those calls as interrupted, as after a crash: one that started there
    /// is reported as started with an unknown result, since going back undoes nothing it
    /// did, and the others as not executed. Nothing is written
    /// until the next append, so a session opened again from its storage continues where
    /// its records end. A run returns Busy; an ID that is not in the session returns
    /// EntryNotFound, and the position does not change.
    pub fn branch(&self, entry: Option<&str>) -> Result<(), SessionError> {
        self.idle(|runner| match entry {
            // The transcript's only error is an unknown ID, which the variant names.
            Some(id) => runner
                .transcript_mut()
                .branch(id)
                .map_err(|_| SessionError::EntryNotFound),
            None => {
                runner.transcript_mut().reset_leaf();
                Ok(())
            }
        })?
    }

    /// Read the session while idle, for example to list its entries or find the current
    /// position. `f` runs with the conversation leased, as a run does: other calls get
    /// Busy until it returns, and no lock is held while host code runs. A run returns Busy.
    pub fn with_transcript<T>(&self, f: impl FnOnce(&Transcript) -> T) -> Result<T, SessionError> {
        let mut lease = self.lease(None)?;
        Ok(f(lease.runner().transcript()))
    }

    /// Run `f` on the Runner under the state lock while idle; a run returns Busy, a closed
    /// session Closed. For quick changes of the kernel's own state only: `f` must not call
    /// host code, and what it returns is dropped after the lock is released, so it can hand
    /// back a host object, such as a replaced provider, to drop there. When refused, `f` is
    /// dropped unlocked too.
    fn idle<T>(&self, f: impl FnOnce(&mut Runner) -> T) -> Result<T, SessionError> {
        let mut state = lock(&self.shared.state);
        let State::Idle(runner) = &mut *state else {
            let refusal = state.refusal();
            drop(state);
            drop(f);
            return Err(refusal);
        };
        Ok(f(runner))
    }

    /// Take the Runner for one operation, which `cancel` cancels if it is a run or a
    /// compaction; Busy while another holds it, Closed once the session is closed.
    fn lease(&self, cancel: Option<CancellationToken>) -> Result<Lease, SessionError> {
        let mut state = lock(&self.shared.state);
        match std::mem::replace(&mut *state, State::Leased(cancel.clone())) {
            State::Idle(runner) => Ok(Lease {
                shared: Arc::clone(&self.shared),
                runner: Some(runner),
                cancel,
            }),
            other @ (State::Leased(_) | State::Closing | State::Closed) => {
                let refusal = other.refusal();
                *state = other;
                Err(refusal)
            }
        }
    }
}

/// The lease of the Runner for one operation. It owns a share of the conversation, so a run
/// can outlive the handle that started it.
///
/// Run ownership is restored by Drop even when the operation's caller disappears.
/// Option permits moving the Runner back exactly once; it is Some until Drop.
struct Lease {
    shared: Arc<Shared>,
    runner: Option<Box<Runner>>,
    cancel: Option<CancellationToken>,
}

impl Lease {
    fn runner(&mut self) -> &mut Runner {
        self.runner
            .as_deref_mut()
            .expect("an active lease owns its Runner until Drop")
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
        let runner = self
            .runner
            .take()
            .expect("an active lease returns its Runner exactly once");
        // A session that no longer agrees with its log cannot record another entry. Closing
        // it lets the agent open it again from the storage, which holds what was recorded.
        let unwritable = runner.transcript().lost_its_log();
        let mut state = lock(&self.shared.state);
        let released = match &*state {
            State::Leased(_) if !unwritable => {
                *state = State::Idle(runner);
                None
            }
            // Unwritable, or closed meanwhile: the Runner goes instead of coming back. While a
            // lease lives only `close` changes the state, to `Closing`, so `Idle` and `Closed`
            // do not occur here; closing is the safe end for them too.
            State::Leased(_) | State::Closing | State::Idle(_) | State::Closed => {
                *state = State::Closed;
                Some(runner)
            }
        };
        drop(state);
        // Without the state lock: closing a storage and dropping tools and hooks is host code.
        drop(released);
    }
}
