//! One conversation: its session, its runs and its approval cards, owned by one task.
//!
//! The worker opens a session when a run slot is available and releases it after the idle time
//! without input. It then asks the router to end it; a message already on its way reopens
//! the session instead, so two tasks never hold one session file.
//!
//! The worker decides which messages it answers ([`Server`](super::Server) lists the rules);
//! the others it records in the session as context for the next answer.
//!
//! - `input`: bounded message admission and recording heard context
//! - `commands`: `/cancel`, `/new` and `/compact`
//! - `cards`: the approval cards, from their posting to their closing

mod cards;
mod commands;
mod input;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{AcquireError, OwnedSemaphorePermit, TryAcquireError, mpsc};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use zx_agent::session::SessionError;
use zx_agent::transcript::CustomMessage;

use crate::{Addressee, ApprovalId, Choice, ImChannel, MessageRef, UserId};

use super::admission::{InputBudget, InputPermit};
use super::approval::ApprovalUpdate;
use super::outlet::{Asked, Finished, Opened, Outlet};
use super::received::Receipt;
use super::render::answer_of;
use super::{ServerEvent, SessionKey, Shared};
use cards::Card;
pub(super) use commands::Command;

/// Approval updates waiting for the worker; [`ApprovalEvents`] explains why few ever wait.
const UPDATE_CAPACITY: usize = 16;

/// A message for the conversation, as the router passes it on.
pub(super) struct Incoming {
    /// Where it was posted, for an acknowledgement.
    pub(super) message: MessageRef,
    pub(super) sender: UserId,
    pub(super) addressee: Addressee,
    pub(super) text: String,
    pub(super) permit: Option<InputPermit>,
    /// Its place in the backlog, kept with it until it is handled.
    pub(super) receipt: Receipt,
}

pub(super) enum WorkerInput {
    Message(Incoming),
    Decision {
        user: UserId,
        approval: ApprovalId,
        choice: Choice,
    },
}

/// Sent by an idle worker that holds no session.
pub(super) struct Retire {
    pub(super) key: SessionKey,
    /// Input the worker has taken; the router ends it only if it has sent no more.
    pub(super) received: u64,
}

pub(super) struct WorkerSetup {
    pub(super) key: SessionKey,
    pub(super) channel: Arc<dyn ImChannel>,
    pub(super) shared: Arc<Shared>,
    pub(super) inbox: mpsc::Receiver<WorkerInput>,
    pub(super) budget: Arc<InputBudget>,
    pub(super) retire_to: mpsc::Sender<Retire>,
    pub(super) stop: CancellationToken,
}

/// Run one conversation until the router drops its inbox.
///
/// A run in progress is cancelled by the stop, and its status then says so
/// ([`Replies::stopped`]). Input does not run once the server stops, nor is it written into
/// the session: the worker reads its inbox to the end, and every message it has not handled
/// stays in the backlog, for the next start.
pub(super) async fn run(setup: WorkerSetup) {
    let mut worker = Worker::new(setup);
    loop {
        if worker.lifecycle == Lifecycle::Draining && matches!(worker.phase, Phase::Idle { .. }) {
            break;
        }
        tokio::select! {
            // Updates first: a card is registered, and closed, before the run that asked ends.
            biased;
            Some(update) = worker.updated.recv() => worker.approval(update).await,
            () = worker.stop.cancelled(), if !worker.lifecycle.closing() => {
                worker.lifecycle = Lifecycle::Stopping;
                // Input waiting for a slot will not run. A run stops through its token and
                // ends as usual.
                worker.stop_waiting();
            }
            input = worker.inbox.recv(), if worker.lifecycle.inbox_open() => match input {
                Some(input) => {
                    worker.received += 1;
                    if worker.lifecycle == Lifecycle::Retiring {
                        worker.lifecycle = Lifecycle::Serving;
                    }
                    // Input that arrives while the server stops does not run: a message stays
                    // in the backlog, and a click finds its approval ended by the stop.
                    if !worker.lifecycle.closing() {
                        worker.input(input).await;
                    }
                }
                // The router ended this conversation, or the server is stopping.
                None => worker.lifecycle = Lifecycle::Draining,
            },
            // A closing conversation waits for its inbox to close, not for the idle time.
            step = step(&mut worker.phase, worker.out.shared.idle, worker.lifecycle) => match step {
                Step::Slot(slot, waiting) => worker.slot(slot, waiting).await,
                Step::Ended(ended, running) => worker.ended(ended, running).await,
                Step::Idle => worker.retire().await,
            },
        }
    }
}

/// Where the conversation is in its life, as the router and the server see it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    /// Input runs; after the idle time the session is released.
    Serving,
    /// Idle with the session released and the router told; input serves again.
    Retiring,
    /// The server is stopping: input is refused while the inbox stays open.
    Stopping,
    /// The inbox is closed; the worker ends once its run has ended.
    Draining,
}

impl Lifecycle {
    /// Input does not run: the server is stopping, or the router ended the conversation.
    fn closing(self) -> bool {
        matches!(self, Self::Stopping | Self::Draining)
    }

    /// The inbox may still bring input.
    fn inbox_open(self) -> bool {
        self != Self::Draining
    }

    /// Nothing waits for the idle time any more.
    fn ending(self) -> bool {
        self != Self::Serving
    }
}

type Slot = Pin<Box<dyn Future<Output = Result<OwnedSemaphorePermit, AcquireError>> + Send>>;

enum Phase {
    /// No run and no input waiting; the session is held until the idle time, or released.
    Idle {
        since: Instant,
        open: Option<Opened>,
    },
    Waiting(Waiting),
    Running(Running),
}

/// Input waits for a run slot. With nothing asked, the slot is only for opening the session to
/// record what was heard.
struct Waiting {
    /// Keeps the conversation's place in the queue for a slot.
    slot: Slot,
    asked: Vec<Asked>,
    open: Option<Opened>,
    /// The message that asked, which a first session reads the thread up to.
    trigger: Option<MessageRef>,
}

/// A prompt, or `/compact`, runs in its own task and holds a run slot.
struct Running {
    task: JoinHandle<Result<Finished, SessionError>>,
    slot: OwnedSemaphorePermit,
    /// The message that says that the work goes on, replaced by its answer at the end.
    status: Option<MessageRef>,
    open: Opened,
    after: AfterRun,
    /// What was asked once `/cancel` or `/new` stopped the run, which takes no more input: it
    /// runs next, as `after` says.
    asked: Vec<Asked>,
}

/// What follows a run once it has ended, as the input during the run asked.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AfterRun {
    /// What was asked during the run runs next, in the same session.
    Continue,
    /// `/new` cancelled the run: what was asked during it runs in a new session.
    Renew,
    /// `/cancel` cancelled the run, or came as it ended: what was steered into it is dropped,
    /// and what was asked after the cancel runs next, in the same session.
    Cancel,
}

impl Phase {
    /// Idle from now, with the session held or released.
    fn idle(open: Option<Opened>) -> Self {
        Self::Idle {
            since: Instant::now(),
            open,
        }
    }

    /// The phase, leaving the conversation idle without a session in its place; the caller
    /// sets what follows.
    fn take(&mut self) -> Self {
        std::mem::replace(self, Self::idle(None))
    }
}

enum Step {
    /// A run slot came for what waited, or the slots closed.
    Slot(Result<OwnedSemaphorePermit, AcquireError>, Waiting),
    /// The work ended: how, or why it gave no report.
    Ended(Result<Finished, String>, Running),
    Idle,
}

/// What the current phase waits for. The futures stay in `phase` until they are done, so a
/// step that another branch interrupts loses nothing; the phase that waited then goes with the
/// step, leaving the conversation idle without a session until the step's handler sets what
/// follows. Nothing waits for the idle time once the conversation's `lifecycle` ends it.
async fn step(phase: &mut Phase, idle: Duration, lifecycle: Lifecycle) -> Step {
    match phase {
        Phase::Idle { since, .. } => {
            match since.checked_add(idle) {
                Some(deadline) if !lifecycle.ending() => tokio::time::sleep_until(deadline).await,
                // Ending already, or an idle time too long to schedule: nothing to wait for.
                _ => std::future::pending::<()>().await,
            }
            Step::Idle
        }
        Phase::Waiting(waiting) => {
            let slot = waiting.slot.as_mut().await;
            let Phase::Waiting(waiting) = phase.take() else {
                unreachable!("the phase that waited for the slot is still there");
            };
            Step::Slot(slot, waiting)
        }
        Phase::Running(running) => {
            let ended = match (&mut running.task).await {
                // The session refused the work: only this worker prompts it, one run at a
                // time, so never for being busy.
                Ok(ended) => ended.map_err(|error| error.to_string()),
                // The work panicked.
                Err(error) => Err(error.to_string()),
            };
            let Phase::Running(running) = phase.take() else {
                unreachable!("the phase that ran the work is still there");
            };
            Step::Ended(ended, running)
        }
    }
}

/// Why the conversation has no session to run or record in.
enum OpenFailure {
    /// The stop ended the wait: what was asked and heard stays in the backlog.
    Stopped,
    /// The session could not be opened, for this reason.
    Failed(String),
}

struct Worker {
    out: Outlet,
    inbox: mpsc::Receiver<WorkerInput>,
    budget: Arc<InputBudget>,
    /// Input taken from the inbox, for [`Retire::received`].
    received: u64,
    retire_to: mpsc::Sender<Retire>,
    stop: CancellationToken,
    updates: mpsc::Sender<ApprovalUpdate>,
    updated: mpsc::Receiver<ApprovalUpdate>,
    lifecycle: Lifecycle,
    phase: Phase,
    cards: Vec<Card>,
    /// The person whose message the bot took last, as long as nobody else spoke since: their
    /// next message is meant for the bot even without a mention.
    answering: Option<UserId>,
    /// Messages not answered and not yet in the session, which a run holds meanwhile.
    heard: Vec<Heard>,
}

/// What someone said that the bot does not answer, until the session records it.
struct Heard {
    message: CustomMessage,
    /// Its share of the budget and its receipt, which go once it is recorded.
    hold: (InputPermit, Receipt),
}

impl Worker {
    fn new(setup: WorkerSetup) -> Self {
        let WorkerSetup {
            key,
            channel,
            shared,
            inbox,
            budget,
            retire_to,
            stop,
        } = setup;
        let (updates, updated) = mpsc::channel(UPDATE_CAPACITY);
        Self {
            out: Outlet {
                to: key.destination(),
                key,
                channel,
                shared,
            },
            inbox,
            budget,
            received: 0,
            retire_to,
            stop,
            updates,
            updated,
            lifecycle: Lifecycle::Serving,
            phase: Phase::idle(None),
            cards: Vec::new(),
            answering: None,
            heard: Vec::new(),
        }
    }

    /// Run what was `asked`, or only record what was heard when nothing was asked, once a run
    /// slot is free: opening a session sets up what it runs with, which only a slot allows.
    /// `trigger` is the message that asked, which a first session reads the thread up to.
    async fn begin(&mut self, open: Option<Opened>, asked: Vec<Asked>, trigger: Option<MessageRef>) {
        match Arc::clone(&self.out.shared.permits).try_acquire_owned() {
            Ok(slot) => self.open_and_start(open, asked, slot, trigger.as_ref()).await,
            Err(TryAcquireError::NoPermits) => {
                let answers_wait = !asked.is_empty();
                self.phase = Phase::Waiting(Waiting {
                    slot: Box::pin(Arc::clone(&self.out.shared.permits).acquire_owned()),
                    asked,
                    open,
                    trigger,
                });
                if answers_wait {
                    self.out.post(self.out.replies().queued.clone()).await;
                }
            }
            Err(TryAcquireError::Closed) => self.slots_closed(open),
        }
    }

    /// Open the session unless `open` holds it and write what was heard into it, then run
    /// what was `asked` there; with nothing asked, the slot served only to record, and goes.
    async fn open_and_start(
        &mut self,
        open: Option<Opened>,
        asked: Vec<Asked>,
        slot: OwnedSemaphorePermit,
        trigger: Option<&MessageRef>,
    ) {
        let open = match self.opened(open, trigger).await {
            Ok(open) => open,
            Err(OpenFailure::Stopped) => return,
            // Nobody asked, so only the operator hears of it.
            Err(OpenFailure::Failed(error)) if asked.is_empty() => {
                self.out.failed(format!(
                    "cannot open the session; {} message(s) not recorded: {error}",
                    self.heard.len()
                ));
                self.heard.clear();
                return;
            }
            Err(OpenFailure::Failed(error)) => return self.out.cannot_open(&error).await,
        };
        if asked.is_empty() {
            self.phase = Phase::idle(Some(open));
            return;
        }
        let prompt = Asked::joined(asked);
        let run = self.out.run(&open.session, prompt, &self.updates, self.stop.clone());
        self.start(open, slot, run).await;
    }

    /// The conversation's session with what was heard written into it, opened unless `open`
    /// still holds it. Opening waits as long as what it waits for: the platform's calls, each
    /// bounded, and the session's setup.
    async fn opened(&mut self, open: Option<Opened>, trigger: Option<&MessageRef>) -> Result<Opened, OpenFailure> {
        let open = match open.filter(Opened::still_open) {
            Some(open) => open,
            None => tokio::select! {
                () = self.stop.cancelled() => return Err(OpenFailure::Stopped),
                opened = self.out.open(trigger) => opened.map_err(OpenFailure::Failed)?,
            },
        };
        self.record_in(&open.session).await;
        Ok(open)
    }

    /// Post that work started, then do `work`, a prompt's run or `/compact`'s, in a task of
    /// its own, which holds the session and the run slot until it ends.
    async fn start(
        &mut self,
        open: Opened,
        slot: OwnedSemaphorePermit,
        work: impl Future<Output = Result<Finished, SessionError>> + Send + 'static,
    ) {
        let status = self.out.post(self.out.replies().working.clone()).await;
        if let Some(status) = &status {
            // Kept before the work starts: should the process end from here on, the next
            // start replaces this status.
            self.out.shared.keep_status(&self.out.key, status).await;
        }
        self.phase = Phase::Running(Running {
            task: tokio::spawn(work),
            slot,
            status,
            open,
            after: AfterRun::Continue,
            asked: Vec::new(),
        });
    }

    /// Go on with what `waiting` held once its run slot came: run what was asked, or record
    /// what was heard.
    async fn slot(&mut self, slot: Result<OwnedSemaphorePermit, AcquireError>, waiting: Waiting) {
        let Waiting {
            asked, open, trigger, ..
        } = waiting;
        match slot {
            Ok(slot) => self.open_and_start(open, asked, slot, trigger.as_ref()).await,
            Err(_) => self.slots_closed(open),
        }
    }

    /// The server never closes its run slots; should they be closed, what was asked is dropped,
    /// and the operator hears of it.
    fn slots_closed(&mut self, open: Option<Opened>) {
        self.phase = Phase::idle(open);
        self.out.failed("the run slots are closed".into());
    }

    /// Post the answer of the work `running` did, then run what was asked as it ended, after
    /// what was heard meanwhile is written. The work gives up its slot first, so that the next
    /// run waits its turn like any other. After `/new` that input runs in the new session,
    /// after `/cancel` it is dropped, and while the server stops it stays in the backlog, with
    /// what was heard.
    async fn ended(&mut self, ended: Result<Finished, String>, running: Running) {
        let Running {
            slot,
            status,
            open,
            after,
            asked,
            ..
        } = running;
        drop(slot);
        let cancelled = matches!(&ended, Ok(finished) if finished.cancelled());
        let answer = if cancelled && self.lifecycle.closing() {
            // The stop cancels runs through their token. A `/cancel` just before the stop ends
            // the same way and is told the same.
            self.out.replies().stopped.clone()
        } else {
            answer_of(ended, self.out.replies())
        };
        self.out.answer(answer, status).await;
        let (open, leftover) = self.leftover(open);
        if self.lifecycle.closing() {
            // A stopping server writes nothing more: what was asked or heard and not written
            // stays in the backlog, for the next start.
            self.phase = Phase::idle(open);
            return;
        }
        let (open, leftover) = match after {
            AfterRun::Continue => (open, leftover),
            AfterRun::Renew => (self.out.start_over(open, &self.stop).await, leftover),
            AfterRun::Cancel => {
                // The run had ended on its own before the cancel reached it, so its answer
                // does not say what the cancel did.
                if !cancelled {
                    let replies = self.out.replies();
                    let reply = if leftover.is_empty() {
                        replies.nothing_to_cancel.clone()
                    } else {
                        replies.cancelled.clone()
                    };
                    self.out.post(reply).await;
                }
                // Dropping them returns their share of the budget.
                (open, Vec::new())
            }
        };
        // What was asked while the run was being stopped runs after what it left.
        let asked: Vec<Asked> = leftover.into_iter().chain(asked).collect();
        if asked.is_empty() {
            self.phase = Phase::idle(open);
            self.record_heard().await;
        } else {
            // Opening the session for the run writes what was heard first.
            self.begin(open, asked, None).await;
        }
    }

    /// What was asked after the run's last look at the queue, and the session the
    /// conversation keeps: `open`, or none once it was closed under the conversation, as a
    /// session closes itself when it can no longer write its records; it is then let go. What
    /// a closed session had queued was acknowledged, so it is taken back, to be answered in
    /// the session opened again.
    fn leftover(&self, open: Opened) -> (Option<Opened>, Vec<Asked>) {
        match open.leftover() {
            Ok(asked) => (Some(open), asked),
            Err(SessionError::Closed) => {
                let asked = open.taken_back();
                self.let_go(open);
                (None, asked)
            }
            Err(error) => {
                self.out.failed(format!("cannot take the queued input: {error}"));
                (Some(open), Vec::new())
            }
        }
    }

    /// Leave Waiting for Idle, keeping the session: what was asked and waited, or `None`
    /// when the conversation was not waiting.
    fn stop_waiting(&mut self) -> Option<Vec<Asked>> {
        match self.phase.take() {
            Phase::Waiting(waiting) => {
                self.phase = Phase::idle(waiting.open);
                Some(waiting.asked)
            }
            other => {
                self.phase = other;
                None
            }
        }
    }

    /// Release the session and tell the router, which ends the conversation unless a message
    /// is on its way: the conversation retires, or stops when the router no longer reads.
    async fn retire(&mut self) {
        self.release();
        let retire = Retire {
            key: self.out.key.clone(),
            received: self.received,
        };
        self.lifecycle = match self.retire_to.send(retire).await {
            Ok(()) => Lifecycle::Retiring,
            // The router no longer reads: the server is stopping.
            Err(_) => Lifecycle::Stopping,
        };
    }

    /// Let go of the session an idle conversation holds, after the idle time or because it
    /// was closed, and tell the operator: the next message opens it again.
    fn release(&mut self) {
        if let Phase::Idle { open, .. } = &mut self.phase
            && let Some(opened) = open.take()
        {
            self.let_go(opened);
        }
    }

    /// Let go of `open`, which closes the session, and tell the operator: the next message
    /// opens it again.
    fn let_go(&self, open: Opened) {
        drop(open);
        self.out.shared.sink.emit(&ServerEvent::Released {
            key: self.out.key.clone(),
        });
    }
}
