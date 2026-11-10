//! One conversation: its session, its runs and its approval cards, owned by one task.
//!
//! The worker opens the session on the first message and releases it after the idle time
//! without input. It then asks the router to end it; a message already on its way reopens
//! the session instead, so two tasks never hold one session file.
//!
//! The worker decides which messages it answers ([`Server`](super::Server) lists the rules);
//! the others it records in the session as context for the next answer.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{AcquireError, OwnedSemaphorePermit, TryAcquireError, mpsc, oneshot};
use tokio::task::{JoinError, JoinHandle};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::agent_session::AgentSessionError;
use crate::ai::now_ms;
use crate::runner::RunReport;
use crate::session::CustomMessage;

use super::approval::{ApprovalUpdate, Decided};
use super::outlet::{Opened, Outlet, said};
use super::server::Shared;
use super::{
    Addressee, ApprovalId, ApprovalRecord, Choice, ImChannel, MessageRef, Opening, Outbound, ServerEvent, SessionKey,
    UserId,
};

/// Sent alone, cancels the conversation's run.
const CANCEL_COMMAND: &str = "/cancel";
/// Sent alone, starts a new session for the conversation, as chat's `/new` does.
const NEW_COMMAND: &str = "/new";
/// Approval updates waiting for the worker; [`ApprovalEvents`] explains why few ever wait.
const UPDATE_CAPACITY: usize = 16;

/// A message for the conversation, as the router passes it on.
pub(crate) struct Incoming {
    /// Where it was posted, for an acknowledgement.
    pub(crate) message: MessageRef,
    pub(crate) sender: UserId,
    pub(crate) addressee: Addressee,
    pub(crate) text: String,
}

pub(crate) enum WorkerInput {
    Message(Incoming),
    Decision {
        user: UserId,
        approval: ApprovalId,
        choice: Choice,
    },
}

/// Sent by an idle worker that holds no session.
pub(crate) struct Retire {
    pub(crate) key: SessionKey,
    /// Input the worker has taken; the router ends it only if it has sent no more.
    pub(crate) received: u64,
}

pub(crate) struct WorkerSetup {
    pub(crate) key: SessionKey,
    pub(crate) channel: Arc<dyn ImChannel>,
    pub(crate) shared: Arc<Shared>,
    pub(crate) inbox: mpsc::Receiver<WorkerInput>,
    pub(crate) retire: mpsc::Sender<Retire>,
    pub(crate) stop: CancellationToken,
}

/// Run one conversation until the router drops its inbox.
///
/// A run in progress is cancelled by the stop, and its status then says so
/// ([`Replies::stopped`]). Input does not run once the server stops; the worker reads its
/// inbox to the end and posts [`Replies::stopping`] in a thread with messages it will not
/// run.
pub(crate) async fn run(setup: WorkerSetup) {
    let mut worker = Worker::new(setup);
    let mut lifecycle = Lifecycle::Serving;
    loop {
        if lifecycle == Lifecycle::Draining && matches!(worker.phase, Phase::Idle { .. }) {
            break;
        }
        tokio::select! {
            // Updates first: a card is registered, and closed, before the run that asked ends.
            biased;
            Some(update) = worker.updated.recv() => worker.approval(update).await,
            () = worker.stop.cancelled(), if !lifecycle.closing() => {
                lifecycle = Lifecycle::Stopping;
                worker.stopping();
            }
            input = worker.inbox.recv(), if lifecycle.inbox_open() => match input {
                Some(input) => {
                    worker.received += 1;
                    if lifecycle == Lifecycle::Retiring {
                        lifecycle = Lifecycle::Serving;
                    }
                    if lifecycle.closing() {
                        worker.refuse(input);
                    } else {
                        worker.input(input).await;
                    }
                }
                // The router ended this conversation, or the server is stopping.
                None => lifecycle = Lifecycle::Draining,
            },
            // A closing conversation waits for its inbox to close, not for the idle time.
            step = step(&mut worker.phase, worker.out.shared.idle, lifecycle.ending()) => match step {
                Step::Slot(slot) => worker.slot(slot).await,
                Step::Ended(ended) => worker.ended(ended, lifecycle.closing()).await,
                Step::Idle => lifecycle = worker.retire().await,
            },
        }
    }
    worker.answer_unanswered().await;
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
    Idle { since: Instant, open: Option<Opened> },
    /// Input waits for a run slot. The future keeps its place in the queue.
    Waiting {
        slot: Slot,
        texts: Vec<String>,
        open: Opened,
    },
    /// A prompt runs in its own task and holds a run slot; `status` is the message that
    /// says so, replaced by the answer at the end.
    Running {
        task: JoinHandle<Result<RunReport, AgentSessionError>>,
        slot: OwnedSemaphorePermit,
        status: Option<MessageRef>,
        open: Opened,
    },
}

impl Phase {
    /// Idle from now, with the session held or released.
    fn idle(open: Option<Opened>) -> Self {
        Self::Idle {
            since: Instant::now(),
            open,
        }
    }
}

enum Step {
    Slot(Result<OwnedSemaphorePermit, AcquireError>),
    Ended(Result<Result<RunReport, AgentSessionError>, JoinError>),
    Idle,
}

/// What the current phase waits for. The futures stay in `phase`, so a step that another
/// branch interrupts loses nothing. `ending` is set once the conversation retires or closes.
async fn step(phase: &mut Phase, idle: Duration, ending: bool) -> Step {
    match phase {
        Phase::Idle { since, .. } => {
            match since.checked_add(idle) {
                Some(deadline) if !ending => tokio::time::sleep_until(deadline).await,
                // Ending already, or an idle time too long to schedule: nothing to wait for.
                _ => std::future::pending::<()>().await,
            }
            Step::Idle
        }
        Phase::Waiting { slot, .. } => Step::Slot(slot.as_mut().await),
        Phase::Running { task, .. } => Step::Ended(task.await),
    }
}

/// Whether `text` is one of the conversation's commands rather than something to answer.
fn is_command(text: &str) -> bool {
    matches!(text.trim(), CANCEL_COMMAND | NEW_COMMAND)
}

/// A card waiting for its result.
struct Pending {
    id: ApprovalId,
    tool_call_id: String,
    card: MessageRef,
    decision: Decision,
}

/// How far a card's decision has come.
enum Decision {
    /// Nobody has clicked yet; the first approver's click goes here.
    Open(oneshot::Sender<Decided>),
    /// Who decided.
    By(UserId),
    /// A click came after the run stopped waiting; the card closes without a decider.
    Lapsed,
}

struct Worker {
    out: Outlet,
    inbox: mpsc::Receiver<WorkerInput>,
    /// Input taken from the inbox, for [`Retire::received`].
    received: u64,
    retire_to: mpsc::Sender<Retire>,
    stop: CancellationToken,
    updates: mpsc::Sender<ApprovalUpdate>,
    updated: mpsc::Receiver<ApprovalUpdate>,
    phase: Phase,
    pending: Vec<Pending>,
    /// Messages that will not run because the server is stopping.
    unanswered: usize,
    /// The person whose message the bot took last, as long as nobody else spoke since: their
    /// next message is meant for the bot even without a mention.
    answering: Option<UserId>,
    /// Messages not answered and not yet in the session, which a run holds meanwhile.
    heard: Vec<CustomMessage>,
    /// What the next opening opens: a new session after `/new`.
    opening: Opening,
}

impl Worker {
    fn new(setup: WorkerSetup) -> Self {
        let WorkerSetup {
            key,
            channel,
            shared,
            inbox,
            retire,
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
            received: 0,
            retire_to: retire,
            stop,
            updates,
            updated,
            phase: Phase::idle(None),
            pending: Vec::new(),
            unanswered: 0,
            answering: None,
            heard: Vec::new(),
            opening: Opening::Latest,
        }
    }

    async fn input(&mut self, input: WorkerInput) {
        match input {
            WorkerInput::Message(incoming) => self.message(incoming).await,
            WorkerInput::Decision { user, approval, choice } => self.decide(user, approval, choice).await,
        }
    }

    /// Whether the bot answers `incoming`: it mentions the bot (every direct message does), or
    /// it mentions no one and comes from the person the bot just answered.
    fn answers(&self, incoming: &Incoming) -> bool {
        match incoming.addressee {
            Addressee::Bot => true,
            Addressee::Nobody => self.answering.as_ref() == Some(&incoming.sender),
            Addressee::Others => false,
        }
    }

    async fn message(&mut self, incoming: Incoming) {
        if !self.answers(&incoming) {
            // Someone else has the floor now.
            self.answering = None;
            let content = self.out.attributed(&incoming.sender, &incoming.text).await;
            self.heard.push(said(content));
            self.record_heard().await;
            return;
        }
        self.answering = Some(incoming.sender.clone());
        match incoming.text.trim() {
            CANCEL_COMMAND => return self.cancel().await,
            NEW_COMMAND => return self.renew().await,
            _ => {}
        }
        let text = self.out.attributed(&incoming.sender, &incoming.text).await;
        let steered = match &mut self.phase {
            Phase::Running { open, .. } => {
                open.session.steer(text);
                true
            }
            Phase::Waiting { texts, .. } => {
                texts.push(text);
                false
            }
            Phase::Idle { open, .. } => {
                let open = open.take();
                self.begin(open, text, &incoming.message).await;
                false
            }
        };
        if steered {
            self.out.acknowledge(&incoming.message).await;
        }
    }

    /// Write the messages heard but not answered into the session; a run that holds it
    /// leaves them for its end. A released session is opened again for them.
    async fn record_heard(&mut self) {
        if self.heard.is_empty() || matches!(self.phase, Phase::Running { .. }) {
            return;
        }
        if let Phase::Idle { open: None, .. } = self.phase {
            match self.open(None).await {
                Ok(opened) => self.phase = Phase::idle(Some(opened)),
                Err(error) => {
                    self.out.failed(format!(
                        "cannot open the session; {} message(s) not recorded: {error}",
                        self.heard.len()
                    ));
                    self.heard.clear();
                    return;
                }
            }
        }
        let session = match &self.phase {
            Phase::Waiting { open, .. } | Phase::Idle { open: Some(open), .. } => Arc::clone(&open.session),
            // Opened above, and no run started since.
            Phase::Idle { open: None, .. } | Phase::Running { .. } => return,
        };
        for message in std::mem::take(&mut self.heard) {
            if let Err(error) = session.append_custom_message(message).await {
                self.out.failed(format!("cannot record a message: {error}"));
            }
        }
    }

    /// Open the session that [`Self::opening`] names. When the bot is first mentioned in a
    /// thread someone else started, by `trigger`, the new session first records what was said
    /// there before.
    async fn open(&mut self, trigger: Option<&MessageRef>) -> Result<Opened, String> {
        let opening = self.opening;
        let opened = self.out.open(&self.updates, opening).await?;
        self.opening = Opening::Latest;
        if opening == Opening::Latest
            && let Some(trigger) = trigger
        {
            self.out.read_thread(&opened, trigger).await;
        }
        Ok(opened)
    }

    /// `/new`: the conversation's next message opens a new session; a run in progress is
    /// cancelled first, and its end releases the session.
    async fn renew(&mut self) {
        self.opening = Opening::New;
        self.answering = None;
        self.heard.clear();
        if let Phase::Running { open, .. } = &self.phase {
            open.session.abort();
            return;
        }
        self.stop_waiting();
        self.release();
        self.out.post(self.out.replies().new_session.clone()).await;
    }

    /// Open the session unless `open` holds it, then run `text` or wait for a slot. `trigger`
    /// is the message that asked.
    async fn begin(&mut self, open: Option<Opened>, text: String, trigger: &MessageRef) {
        let open = match open {
            Some(open) => open,
            None => match self.open(Some(trigger)).await {
                Ok(open) => open,
                Err(error) => {
                    self.out
                        .post(format!("{}{error}", self.out.replies().cannot_open))
                        .await;
                    self.out.failed(format!("cannot open the session: {error}"));
                    return;
                }
            },
        };
        match Arc::clone(&self.out.shared.permits).try_acquire_owned() {
            Ok(slot) => self.start(open, vec![text], slot).await,
            Err(TryAcquireError::NoPermits) => {
                self.phase = Phase::Waiting {
                    slot: Box::pin(Arc::clone(&self.out.shared.permits).acquire_owned()),
                    texts: vec![text],
                    open,
                };
                self.out.post(self.out.replies().queued.clone()).await;
            }
            // The server never closes its semaphore.
            Err(TryAcquireError::Closed) => {
                self.phase = Phase::idle(Some(open));
                self.out.failed("the run slots are closed".into());
            }
        }
    }

    /// Post that the work started, and run the input as one prompt.
    async fn start(&mut self, open: Opened, texts: Vec<String>, slot: OwnedSemaphorePermit) {
        let status = self.out.post(self.out.replies().working.clone()).await;
        if let Some(status) = &status {
            // Kept before the run starts: should the process end from here on, the next
            // start replaces this status.
            self.out.shared.keep_status(&self.out.key, status).await;
        }
        let session = Arc::clone(&open.session);
        let stop = self.stop.clone();
        let text = texts.join("\n\n");
        let task = tokio::spawn(async move { session.prompt(text, &stop).await });
        self.phase = Phase::Running {
            task,
            slot,
            status,
            open,
        };
    }

    /// Take the phase, leaving Idle without a session; the caller sets what follows.
    fn take_phase(&mut self) -> Phase {
        std::mem::replace(&mut self.phase, Phase::idle(None))
    }

    async fn slot(&mut self, slot: Result<OwnedSemaphorePermit, AcquireError>) {
        let (texts, open) = match self.take_phase() {
            Phase::Waiting { texts, open, .. } => (texts, open),
            // Only a waiting conversation gets a slot.
            other => {
                self.phase = other;
                return;
            }
        };
        match slot {
            Ok(slot) => self.start(open, texts, slot).await,
            // The server never closes its semaphore.
            Err(_) => {
                self.phase = Phase::idle(Some(open));
                self.out.failed("the run slots are closed".into());
            }
        }
    }

    /// Post the answer and record what was heard meanwhile, then run input that arrived as
    /// the run ended; while the server stops, that input is unanswered instead. After `/new`
    /// the session is released instead.
    async fn ended(&mut self, ended: Result<Result<RunReport, AgentSessionError>, JoinError>, closing: bool) {
        let (slot, status, open) = match self.take_phase() {
            Phase::Running { slot, status, open, .. } => (slot, status, open),
            // Only a running conversation ends a run.
            other => {
                self.phase = other;
                return;
            }
        };
        self.out.answer(ended, status, closing).await;
        if self.opening == Opening::New {
            self.phase = Phase::idle(Some(open));
            self.release();
            self.out.post(self.out.replies().new_session.clone()).await;
            return;
        }
        let leftover = match open.leftover_texts() {
            Ok(texts) => texts,
            Err(error) => {
                self.out.failed(error);
                Vec::new()
            }
        };
        self.phase = Phase::idle(Some(open));
        self.record_heard().await;
        if closing || leftover.is_empty() {
            self.unanswered += leftover.len();
            return;
        }
        // Recording what was heard keeps the session that is open.
        if let Phase::Idle { open: Some(open), .. } = self.take_phase() {
            self.start(open, leftover, slot).await;
        }
    }

    /// Leave Waiting for Idle, keeping the session: the texts that waited, or `None` when
    /// the conversation was not waiting.
    fn stop_waiting(&mut self) -> Option<Vec<String>> {
        match self.take_phase() {
            Phase::Waiting { texts, open, .. } => {
                self.phase = Phase::idle(Some(open));
                Some(texts)
            }
            other => {
                self.phase = other;
                None
            }
        }
    }

    /// The server is stopping: input waiting for a slot will not run. A run stops through
    /// its token and ends as usual.
    fn stopping(&mut self) {
        if let Some(texts) = self.stop_waiting() {
            self.unanswered += texts.len();
        }
    }

    /// Input that arrives while the server stops does not run. Only a message the bot would
    /// have answered counts as unanswered.
    fn refuse(&mut self, input: WorkerInput) {
        match input {
            WorkerInput::Message(incoming) if self.answers(&incoming) && !is_command(&incoming.text) => {
                self.unanswered += 1;
            }
            // The stop cancels the run already, approvals included, and drops what waits.
            WorkerInput::Message(_) | WorkerInput::Decision { .. } => {}
        }
    }

    /// Release the session and tell the router, which ends the conversation unless a message
    /// is on its way: the conversation retires, or stops when the router no longer reads.
    async fn retire(&mut self) -> Lifecycle {
        self.release();
        let retire = Retire {
            key: self.out.key.clone(),
            received: self.received,
        };
        match self.retire_to.send(retire).await {
            Ok(()) => Lifecycle::Retiring,
            // The router no longer reads: the server is stopping.
            Err(_) => Lifecycle::Stopping,
        }
    }

    /// Tell the thread that its remaining messages will not run. Called as the conversation
    /// ends, so it follows how the run ended.
    async fn answer_unanswered(self) {
        if self.unanswered == 0 {
            return;
        }
        self.out.post(self.out.replies().stopping.clone()).await;
        self.out.failed(format!(
            "{} message(s) were not answered: the server is stopping",
            self.unanswered
        ));
    }

    async fn cancel(&mut self) {
        if let Phase::Running { open, .. } = &self.phase {
            // The run ends as aborted, and its status message then says so.
            open.session.abort();
            return;
        }
        let text = match self.stop_waiting() {
            Some(_) => self.out.replies().cancelled.clone(),
            None => self.out.replies().nothing_to_cancel.clone(),
        };
        self.out.post(text).await;
    }

    async fn decide(&mut self, user: UserId, approval: ApprovalId, choice: Choice) {
        let Some(index) = self.pending.iter().position(|pending| pending.id == approval) else {
            // The card was closed already; a late click changes nothing.
            return;
        };
        if !self.out.shared.is_approver(&self.out.key.platform, &user) {
            self.out.post(self.out.replies().not_approver.clone()).await;
            return;
        }
        let pending = &mut self.pending[index];
        // The first approver's click decides; later clicks on the card change nothing.
        let Decision::Open(decide) = std::mem::replace(&mut pending.decision, Decision::Lapsed) else {
            return;
        };
        // Sending fails when confirm stopped waiting (timeout or cancellation); the run's
        // approval_resolved event then closes the card without a decider.
        let decided = Decided {
            user: user.clone(),
            choice,
        };
        if decide.send(decided).is_ok() {
            pending.decision = Decision::By(user);
        }
    }

    async fn approval(&mut self, update: ApprovalUpdate) {
        match update {
            ApprovalUpdate::Posted(posted) => self.pending.push(Pending {
                id: posted.id,
                tool_call_id: posted.tool_call_id,
                card: posted.card,
                decision: Decision::Open(posted.decide),
            }),
            ApprovalUpdate::Resolved {
                tool_call_id,
                tool,
                result,
            } => {
                let pending = self
                    .pending
                    .iter()
                    .position(|pending| pending.tool_call_id == tool_call_id)
                    .map(|index| self.pending.swap_remove(index));
                let (approval, user) = match pending {
                    Some(pending) => {
                        let user = match pending.decision {
                            Decision::By(user) => Some(user),
                            Decision::Open(_) | Decision::Lapsed => None,
                        };
                        let closed = Outbound::ApprovalResolved {
                            id: pending.id.clone(),
                            user: user.clone(),
                            result,
                        };
                        self.out.replace(&pending.card, closed).await;
                        (Some(pending.id), user)
                    }
                    // No card: posting it failed, so the approval ended without one.
                    None => (None, None),
                };
                self.out.shared.sink.emit(&ServerEvent::Approval(ApprovalRecord {
                    key: self.out.key.clone(),
                    tool_call_id,
                    tool,
                    approval,
                    user,
                    result,
                    ended_ms: now_ms(),
                }));
            }
        }
    }

    /// Release the session after the idle time, freeing its file for the next opening.
    fn release(&mut self) {
        if let Phase::Idle { open, .. } = &mut self.phase
            && let Some(opened) = open.take()
        {
            drop(opened);
            self.out.shared.sink.emit(&ServerEvent::Released {
                key: self.out.key.clone(),
            });
        }
    }
}
