//! A conversation, shared by its frontends, and its runs.
//!
//! - `run`: [`Run`], one prompt, continuation or compaction in progress: its events and its
//!   result
//!
//! A run takes ownership of the Runner, without a lock across await. Its lease restores the
//! Runner when the run ends or is dropped, also by a panic.

mod run;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::ai::retry::RetryPolicy;
use crate::ai::{Provider, Reasoning};
use crate::compaction;
use crate::events::Event;
use crate::extensions::{CommandContext, CommandError, CommandOutcome, SlashCommand};
use crate::input::{Input, InputQueues};
use crate::policy::{ApprovalDecision, ApprovalGate};
use crate::runner::{LoopConfig, RunLimits, RunLink, Runner, Start};
use crate::sync::lock;
use crate::tools::ActiveTools;
use crate::transcript::{CustomMessage, SessionName, Transcript};

use run::EVENT_BUFFER;
pub use run::Run;

/// Provider and model selected together, so a switch cannot expose a mixed pair.
pub struct ModelRef {
    /// Already configured by the host, including authentication and transport limits.
    pub provider: Arc<dyn Provider>,
    /// Non-empty model ID passed to that provider on subsequent prompts.
    pub model: String,
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
    /// The session could not record an entry; see [`Transcript::writes_stopped`].
    #[error("cannot write the session: {0}")]
    WriteFailed(String),
    /// No approval request with that ID waits for an answer: it was answered already, nobody
    /// answered it in time, or its run was cancelled.
    #[error("no approval request with that ID is waiting")]
    UnknownApproval,
}

enum State {
    // A full Runner is only needed while idle, so keep the state enum small.
    Idle(Box<Runner>),
    /// An operation holds the Runner; the token cancels it.
    Leased(CancellationToken),
}

/// One long-lived conversation. A handle: its clones are the same conversation, so that a
/// frontend can read a run in one task and cancel or steer it from another. State locks
/// cover ownership transfers only; host code and asynchronous work run unlocked.
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
    cwd: PathBuf,
    id: String,
}

impl Session {
    /// A conversation recorded in `transcript`. Fails for compaction budgets that cannot
    /// work, without making a request or changing the session.
    pub fn new(
        config: LoopConfig,
        retry: RetryPolicy,
        limits: RunLimits,
        compaction: compaction::Config,
        transcript: Transcript,
    ) -> anyhow::Result<Self> {
        let runner = Runner::new(config, retry, limits, transcript).with_compaction(compaction)?;
        let config = runner.config();
        Ok(Self {
            shared: Arc::new(Shared {
                queues: config.queues.clone(),
                active_tools: config.active_tools.clone(),
                approvals: runner.approvals().clone(),
                commands: config.commands.clone(),
                cwd: config.cwd.clone(),
                id: runner.session().header().id.clone(),
                state: Mutex::new(State::Idle(Box::new(runner))),
            }),
        })
    }

    /// The session's ID, as its header records it.
    pub fn id(&self) -> &str {
        &self.shared.id
    }

    /// Start a run with `input`: the person's text, or a message from an extension (pi:
    /// `sendMessage` with `triggerTurn` while idle). This only takes the conversation; the
    /// returned [`Run`] makes the requests as it is read. An overlap returns Busy.
    pub fn send(&self, input: impl Into<Input>) -> Result<Run, SessionError> {
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

    fn start(&self, begin: Start) -> Result<Run, SessionError> {
        let (mut lease, link, events) = self.begin()?;
        let id = link.id.clone();
        Ok(Run::new(
            id,
            events,
            async move { lease.runner().run(begin, &link).await },
        ))
    }

    /// Summarize the old prefix of the conversation without executing a task or tools, as a
    /// run of its own, with the same exclusive hold on the conversation as a prompt. An
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
        let lease = self.lease(cancel.clone())?;
        let (events, reader) = mpsc::channel(EVENT_BUFFER);
        let link = RunLink {
            id: uuid::Uuid::new_v4().to_string(),
            cancel,
            events,
        };
        Ok((lease, link, reader))
    }

    /// The conversation's queues: to steer or follow up with the person's input (`steer`,
    /// `follow_up`) or a message from an extension (`steer_message`, `follow_up_message`), to
    /// record extension state during a run (`append_entry`), and to choose how many queued
    /// messages a request takes (`set_mode`).
    pub fn queues(&self) -> &InputQueues {
        &self.shared.queues
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
        let shared = &self.shared;
        let command = shared
            .commands
            .iter()
            .find(|command| command.name() == name)
            .ok_or(CommandError::Unknown)?;
        let ctx = CommandContext::new(
            shared.cwd.clone(),
            shared.id.clone(),
            shared.queues.clone(),
            shared.active_tools.clone(),
        );
        command.run(args, &ctx).await.map_err(CommandError::Failed)
    }

    /// Record state of an extension in the session while idle (pi: `appendEntry`); during
    /// a run, hooks and commands use `queues().append_entry` instead. Returns the entry's
    /// ID. A run returns Busy; a write failure returns WriteFailed.
    pub async fn append_entry(&self, custom_type: &str, data: Value) -> Result<String, SessionError> {
        self.append_idle(async |store| store.append_custom_entry(custom_type, data).await)
            .await
    }

    /// Record a message from an extension while idle, without starting a run (pi:
    /// `sendMessage` while idle): the model sees it with the next prompt. Returns the
    /// entry's ID. A run returns Busy; a write failure returns WriteFailed.
    pub async fn append_custom_message(&self, message: CustomMessage) -> Result<String, SessionError> {
        self.append_idle(async |store| store.append_custom_message(message).await)
            .await
    }

    /// Name the session while idle (pi: `setSessionName`): listings show the name instead of
    /// the first prompt, and the model never sees it. The latest name counts, whichever branch
    /// it was given on; `with_transcript` with [`Transcript::session_name`] reads it. Returns
    /// the entry's ID. A run returns Busy; a write failure returns WriteFailed.
    pub async fn set_name(&self, name: SessionName) -> Result<String, SessionError> {
        self.append_idle(async |store| store.append_session_name(name).await)
            .await
    }

    /// Write one record with `append` while idle, with the conversation leased as for a
    /// run. A run returns Busy; a write failure returns WriteFailed.
    async fn append_idle(
        &self,
        append: impl AsyncFnOnce(&mut Transcript) -> anyhow::Result<String>,
    ) -> Result<String, SessionError> {
        let mut lease = self.lease(CancellationToken::new())?;
        append(lease.runner().session_mut())
            .await
            .map_err(|error| SessionError::WriteFailed(format!("{error:#}")))
    }

    /// Take the input that `queues().steer` and `follow_up` queued and no run consumed: steering
    /// messages first, then follow-ups, each oldest first. A run looks for input at the end
    /// of each step, so input queued as it ends waits for the next run; a host that starts
    /// runs itself calls this when one ends and sends what it gets. Returns Busy during a
    /// run or compaction, whose loop may still take the input.
    pub fn take_queued(&self) -> Result<Vec<Input>, SessionError> {
        // Holding the state lock keeps a run from starting between the check and the take.
        self.idle(|_| self.shared.queues.take_all())
    }

    /// Cancel the run or compaction in progress; its [`Run`] then ends with a cancelled
    /// result. Returns false when idle. The short operations that read or write the session
    /// while idle (`with_transcript`, `append_entry`, `append_custom_message`, `set_name`)
    /// hold the conversation as well: during one of them this returns true, although they
    /// have nothing to cancel.
    pub fn cancel(&self) -> bool {
        let cancel = match &*lock(&self.shared.state) {
            State::Idle(_) => return false,
            State::Leased(cancel) => cancel.clone(),
        };
        // Waking a waiting future can call host-provided wakers; do it unlocked.
        cancel.cancel();
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

    /// Switch provider and model while idle. Empty IDs return InvalidModel; a run returns
    /// Busy. History, tools, hooks, limits and exact-call approval grants are retained.
    /// No request is made and the session header keeps its original starting model.
    pub fn set_model(&self, model: ModelRef) -> Result<(), SessionError> {
        if model.model.trim().is_empty() {
            return Err(SessionError::InvalidModel);
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
    pub fn set_reasoning(&self, reasoning: Option<Reasoning>) -> Result<(), SessionError> {
        self.idle(|runner| {
            runner.replace_reasoning(reasoning);
        })
    }

    /// Move the conversation to another point of its session tree while idle (pi:
    /// `navigateTree`): the next run continues after `entry`, or before the first entry
    /// for `None`, on a new branch there. The entries of the branch left behind stay in the
    /// session, and a run after an answer whose tool calls have their results on that
    /// branch records those calls as not executed, as after a crash. Nothing is written
    /// until the next append, so a session opened again from its storage continues where
    /// its records end. A run returns Busy; an ID that is not in the session returns
    /// EntryNotFound, and the position does not change.
    pub fn branch(&self, entry: Option<&str>) -> Result<(), SessionError> {
        self.idle(|runner| match entry {
            // The store's only error is an unknown ID, which the variant names.
            Some(id) => runner.session_mut().branch(id).map_err(|_| SessionError::EntryNotFound),
            None => {
                runner.session_mut().reset_leaf();
                Ok(())
            }
        })?
    }

    /// Read the session while idle, for example to list its entries or find the current
    /// position. `f` runs with the conversation leased, as a run does: other calls get
    /// Busy until it returns, and no lock is held while host code runs. A run returns Busy.
    pub fn with_transcript<T>(&self, f: impl FnOnce(&Transcript) -> T) -> Result<T, SessionError> {
        let mut lease = self.lease(CancellationToken::new())?;
        Ok(f(lease.runner().session()))
    }

    /// Run `f` on the Runner under the state lock while idle; a run returns Busy. For quick
    /// changes of the kernel's own state only: `f` must not call host code, and what it
    /// returns is dropped after the lock is released, so it can hand back a host object,
    /// such as a replaced provider, to drop there. When busy, `f` is dropped unlocked too.
    fn idle<T>(&self, f: impl FnOnce(&mut Runner) -> T) -> Result<T, SessionError> {
        let mut state = lock(&self.shared.state);
        let State::Idle(runner) = &mut *state else {
            drop(state);
            drop(f);
            return Err(SessionError::Busy);
        };
        Ok(f(runner))
    }

    /// Take the Runner for one operation that `cancel` cancels; Busy while another holds it.
    fn lease(&self, cancel: CancellationToken) -> Result<Lease, SessionError> {
        let mut state = lock(&self.shared.state);
        match std::mem::replace(&mut *state, State::Leased(cancel.clone())) {
            State::Idle(runner) => Ok(Lease {
                shared: Arc::clone(&self.shared),
                runner: Some(runner),
                cancel,
            }),
            busy @ State::Leased(_) => {
                *state = busy;
                Err(SessionError::Busy)
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
    cancel: CancellationToken,
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
        self.cancel.cancel();
        let runner = self
            .runner
            .take()
            .expect("an active lease returns its Runner exactly once");
        *lock(&self.shared.state) = State::Idle(runner);
    }
}
