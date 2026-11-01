//! One conversation: its session, its runs and its approval cards, owned by one task.
//!
//! The worker opens the session on the first message and releases it after the idle time
//! without input. It then asks the router to end it; a message already on its way reopens
//! the session instead, so two tasks never hold one session file.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{AcquireError, OwnedSemaphorePermit, TryAcquireError, mpsc, oneshot};
use tokio::task::{JoinError, JoinHandle};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::AgentSession;
use crate::agent_session::{SessionError, Subscription};
use crate::ai::now_ms;
use crate::runner::{RunOutcome, RunReport};

use super::approval::{ApprovalEvents, ApprovalUpdate, Decided, ImApproval};
use super::server::Shared;
use super::{
    ApprovalId, ApprovalRecord, Choice, Destination, ImChannel, MessageRef, Outbound, Replies, ServerEvent, SessionKey,
    UserId,
};

/// Sent alone in a thread, cancels its run.
const CANCEL_COMMAND: &str = "/cancel";
/// Approval updates waiting for the worker; [`ApprovalEvents`] explains why few ever wait.
const UPDATE_CAPACITY: usize = 16;

pub(crate) enum WorkerInput {
    Message {
        text: String,
    },
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

/// Run one conversation until the router drops its inbox or the server stops. A run in
/// progress is cancelled by the stop and still posts how it ended.
pub(crate) async fn run(setup: WorkerSetup) {
    let WorkerSetup {
        key,
        channel,
        shared,
        mut inbox,
        retire,
        stop,
    } = setup;
    let (updates, mut updated) = mpsc::channel(UPDATE_CAPACITY);
    let mut worker = Worker {
        out: Outlet {
            to: key.destination(),
            key,
            channel,
            shared,
        },
        retire,
        stop,
        updates,
        open: None,
        phase: Phase::Idle { since: Instant::now() },
        pending: Vec::new(),
    };
    let mut received: u64 = 0;
    let mut retiring = false;
    let mut closing = false;
    loop {
        if closing && matches!(worker.phase, Phase::Idle { .. }) {
            break;
        }
        tokio::select! {
            // Updates first: a card is registered, and closed, before the run that asked ends.
            biased;
            Some(update) = updated.recv() => worker.approval(update).await,
            () = worker.stop.cancelled(), if !closing => {
                closing = true;
                // Input waiting for a slot is dropped; a run stops through its token.
                if matches!(worker.phase, Phase::Waiting { .. }) {
                    worker.phase = Phase::Idle { since: Instant::now() };
                }
            }
            input = inbox.recv(), if !closing => match input {
                Some(input) => {
                    received += 1;
                    retiring = false;
                    worker.input(input).await;
                }
                // The router ended this conversation, or the server is stopping.
                None => closing = true,
            },
            step = step(&mut worker.phase, worker.out.shared.idle, retiring) => match step {
                Step::Slot(slot) => worker.slot(slot).await,
                Step::Ended(ended) => worker.ended(ended, closing).await,
                Step::Idle => {
                    worker.release();
                    retiring = true;
                    let retire = Retire { key: worker.out.key.clone(), received };
                    if worker.retire.send(retire).await.is_err() {
                        // The router no longer reads: the server is stopping.
                        closing = true;
                    }
                }
            },
        }
    }
}

/// The session while it is held. Dropping it releases the session file.
struct Opened {
    // Shared with the task that runs the current prompt, as AgentSession is meant to be:
    // meanwhile the worker steers, aborts and takes queued input through it.
    session: Arc<AgentSession>,
    _approvals: Subscription,
}

type Slot = Pin<Box<dyn Future<Output = Result<OwnedSemaphorePermit, AcquireError>> + Send>>;

enum Phase {
    /// No run and no input waiting; the session may be open or released.
    Idle { since: Instant },
    /// Input waits for a run slot. The future keeps its place in the queue.
    Waiting { slot: Slot, texts: Vec<String> },
    /// A prompt runs in its own task and holds a run slot; `status` is the message that
    /// says so, replaced by the answer at the end.
    Running {
        task: JoinHandle<Result<RunReport, SessionError>>,
        slot: OwnedSemaphorePermit,
        status: Option<MessageRef>,
    },
}

enum Step {
    Slot(Result<OwnedSemaphorePermit, AcquireError>),
    Ended(Result<Result<RunReport, SessionError>, JoinError>),
    Idle,
}

/// What the current phase waits for. The futures stay in `phase`, so a step that another
/// branch interrupts loses nothing.
async fn step(phase: &mut Phase, idle: Duration, retiring: bool) -> Step {
    match phase {
        Phase::Idle { since } => {
            match since.checked_add(idle) {
                Some(deadline) if !retiring => tokio::time::sleep_until(deadline).await,
                // Retiring already, or an idle time too long to schedule: nothing to wait for.
                _ => std::future::pending::<()>().await,
            }
            Step::Idle
        }
        Phase::Waiting { slot, .. } => Step::Slot(slot.as_mut().await),
        Phase::Running { task, .. } => Step::Ended(task.await),
    }
}

/// A card waiting for its result.
struct Pending {
    id: ApprovalId,
    tool_call_id: String,
    card: MessageRef,
    /// Until someone decides.
    decide: Option<oneshot::Sender<Decided>>,
    /// Who decided, once someone has.
    user: Option<UserId>,
}

/// Where the conversation posts and reports. Separate from [`Worker`] so that posting
/// borrows only these fields while the worker's own state stays with the worker.
struct Outlet {
    key: SessionKey,
    to: Destination,
    channel: Arc<dyn ImChannel>,
    shared: Arc<Shared>,
}

impl Outlet {
    /// Post text in the thread. A failure is reported to the operator and returns `None`.
    async fn post(&self, text: String) -> Option<MessageRef> {
        let message = Outbound::Text(text);
        match tokio::time::timeout(self.shared.outbound_timeout, self.channel.send(&self.to, &message)).await {
            Ok(Ok(posted)) => Some(posted),
            Ok(Err(error)) => {
                self.failed(format!("cannot post: {error}"));
                None
            }
            Err(_) => {
                self.failed(format!(
                    "posting took longer than {} s",
                    self.shared.outbound_timeout.as_secs()
                ));
                None
            }
        }
    }

    /// Replace a posted message. A failure is reported to the operator.
    async fn replace(&self, posted: &MessageRef, message: Outbound) {
        match tokio::time::timeout(self.shared.outbound_timeout, self.channel.update(posted, &message)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => self.failed(format!("cannot update message {}: {error}", posted.id)),
            Err(_) => self.failed(format!(
                "updating message {} took longer than {} s",
                posted.id,
                self.shared.outbound_timeout.as_secs()
            )),
        }
    }

    /// Open the session, with this thread as the host of its approvals.
    async fn open(&self, updates: &mpsc::Sender<ApprovalUpdate>) -> Result<Opened, String> {
        let interaction = Arc::new(ImApproval {
            channel: Arc::clone(&self.channel),
            to: self.to.clone(),
            updates: updates.clone(),
            outbound_timeout: self.shared.outbound_timeout,
        });
        let session = self.shared.factory.open(&self.key, interaction).await?;
        let approvals = session.subscribe(Arc::new(ApprovalEvents {
            updates: updates.clone(),
            key: self.key.clone(),
            sink: Arc::clone(&self.shared.sink),
        }));
        Ok(Opened {
            session: Arc::new(session),
            _approvals: approvals,
        })
    }

    fn failed(&self, error: String) {
        self.shared.failed(Some(&self.key), error);
    }

    fn replies(&self) -> &Replies {
        &self.shared.replies
    }
}

struct Worker {
    out: Outlet,
    retire: mpsc::Sender<Retire>,
    stop: CancellationToken,
    updates: mpsc::Sender<ApprovalUpdate>,
    open: Option<Opened>,
    phase: Phase,
    pending: Vec<Pending>,
}

impl Worker {
    async fn input(&mut self, input: WorkerInput) {
        match input {
            WorkerInput::Message { text } => self.message(text).await,
            WorkerInput::Decision { user, approval, choice } => self.decide(user, approval, choice).await,
        }
    }

    async fn message(&mut self, text: String) {
        if text.trim() == CANCEL_COMMAND {
            self.cancel().await;
            return;
        }
        match &mut self.phase {
            Phase::Running { .. } => {
                if let Some(open) = &self.open {
                    open.session.steer(text);
                }
            }
            Phase::Waiting { texts, .. } => texts.push(text),
            Phase::Idle { .. } => self.begin(text).await,
        }
    }

    /// Open the session if needed, then run `text` or wait for a slot.
    async fn begin(&mut self, text: String) {
        if self.open.is_none() {
            match self.out.open(&self.updates).await {
                Ok(opened) => self.open = Some(opened),
                Err(error) => {
                    self.out
                        .post(format!("{}{error}", self.out.replies().cannot_open))
                        .await;
                    self.out.failed(format!("cannot open the session: {error}"));
                    return;
                }
            }
        }
        match Arc::clone(&self.out.shared.permits).try_acquire_owned() {
            Ok(slot) => self.start(vec![text], slot).await,
            Err(TryAcquireError::NoPermits) => {
                self.phase = Phase::Waiting {
                    slot: Box::pin(Arc::clone(&self.out.shared.permits).acquire_owned()),
                    texts: vec![text],
                };
                self.out.post(self.out.replies().queued.clone()).await;
            }
            // The server never closes its semaphore.
            Err(TryAcquireError::Closed) => self.out.failed("the run slots are closed".into()),
        }
    }

    /// Post that the work started, and run the input as one prompt.
    async fn start(&mut self, texts: Vec<String>, slot: OwnedSemaphorePermit) {
        let session = match &self.open {
            Some(open) => Arc::clone(&open.session),
            // Callers open the session first; without it there is nothing to run.
            None => return,
        };
        let status = self.out.post(self.out.replies().working.clone()).await;
        let stop = self.stop.clone();
        let text = texts.join("\n\n");
        let task = tokio::spawn(async move { session.prompt_with_cancel(text, &stop).await });
        self.phase = Phase::Running { task, slot, status };
    }

    async fn slot(&mut self, slot: Result<OwnedSemaphorePermit, AcquireError>) {
        let Phase::Waiting { texts, .. } = std::mem::replace(&mut self.phase, Phase::Idle { since: Instant::now() })
        else {
            return;
        };
        match slot {
            Ok(slot) => self.start(texts, slot).await,
            // The server never closes its semaphore.
            Err(_) => self.out.failed("the run slots are closed".into()),
        }
    }

    /// Post the answer, then run input that arrived as the run ended.
    async fn ended(&mut self, ended: Result<Result<RunReport, SessionError>, JoinError>, closing: bool) {
        let Phase::Running { slot, status, .. } =
            std::mem::replace(&mut self.phase, Phase::Idle { since: Instant::now() })
        else {
            return;
        };
        let answer = match ended {
            Ok(Ok(report)) => render(&report, self.out.replies()),
            // Only this worker prompts the session, one run at a time, so Busy cannot occur.
            Ok(Err(error)) => format!("{}{error}", self.out.replies().failed),
            Err(error) => format!("{}{error}", self.out.replies().failed),
        };
        match status {
            Some(posted) => self.out.replace(&posted, Outbound::Text(answer)).await,
            None => {
                self.out.post(answer).await;
            }
        }
        // The loop looks for input at the end of each turn, so a message steered after the
        // last look waits in the queue (docs/serve-im-review.md, gap 3).
        let leftover = match self.open.as_ref().map(|open| open.session.take_queued()) {
            Some(Ok(texts)) => texts,
            Some(Err(error)) => {
                self.out.failed(format!("cannot take the queued input: {error}"));
                Vec::new()
            }
            None => Vec::new(),
        };
        if leftover.is_empty() {
            return;
        }
        if closing {
            self.out.failed(format!(
                "{} queued message(s) were not answered: the server is stopping",
                leftover.len()
            ));
            return;
        }
        self.start(leftover, slot).await;
    }

    async fn cancel(&mut self) {
        if matches!(self.phase, Phase::Running { .. }) {
            // The run ends as aborted, and its status message then says so.
            if let Some(open) = &self.open {
                open.session.abort();
            }
            return;
        }
        let text = if matches!(self.phase, Phase::Waiting { .. }) {
            self.phase = Phase::Idle { since: Instant::now() };
            self.out.replies().cancelled.clone()
        } else {
            self.out.replies().nothing_to_cancel.clone()
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
        let Some(decide) = pending.decide.take() else {
            return;
        };
        // Sending fails when confirm stopped waiting (timeout or cancellation); the run's
        // approval_resolved event then closes the card without a decider.
        if decide
            .send(Decided {
                user: user.clone(),
                choice,
            })
            .is_ok()
        {
            pending.user = Some(user);
        }
    }

    async fn approval(&mut self, update: ApprovalUpdate) {
        match update {
            ApprovalUpdate::Posted(posted) => self.pending.push(Pending {
                id: posted.id,
                tool_call_id: posted.tool_call_id,
                card: posted.card,
                decide: Some(posted.decide),
                user: None,
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
                        let closed = Outbound::ApprovalResolved {
                            id: pending.id.clone(),
                            user: pending.user.clone(),
                            result,
                        };
                        self.out.replace(&pending.card, closed).await;
                        (Some(pending.id), pending.user)
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
        let released = self.open.take();
        if let Some(opened) = released {
            drop(opened);
            self.out.shared.sink.emit(&ServerEvent::Released {
                key: self.out.key.clone(),
            });
        }
    }
}

/// The text that replaces the status message at the end of a run.
fn render(report: &RunReport, replies: &Replies) -> String {
    let answer = report.final_text.trim();
    let noted = |note: &str| {
        if answer.is_empty() {
            note.to_string()
        } else {
            format!("{answer}\n\n{note}")
        }
    };
    match report.outcome {
        RunOutcome::Completed if answer.is_empty() => replies.no_answer.clone(),
        RunOutcome::Completed => answer.to_string(),
        RunOutcome::Truncated => noted(&replies.truncated),
        RunOutcome::MaxTurns => noted(&replies.max_turns),
        // The last answer in the session may be an earlier run's, so it is not repeated.
        RunOutcome::Timeout => replies.timed_out.clone(),
        RunOutcome::Aborted => replies.cancelled.clone(),
        RunOutcome::Error => match &report.error {
            Some(error) => format!("{}{error}", replies.failed),
            None => replies.failed.trim_end_matches('：').to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::Usage;

    fn report(outcome: RunOutcome, final_text: &str, error: Option<&str>) -> RunReport {
        RunReport {
            outcome,
            final_text: final_text.into(),
            turns: 1,
            retries: 0,
            usage: Usage::default(),
            error: error.map(String::from),
            session_file: None,
        }
    }

    #[test]
    fn a_completed_run_posts_its_answer() {
        let replies = Replies::default();
        assert_eq!(
            render(&report(RunOutcome::Completed, " done \n", None), &replies),
            "done"
        );
        assert_eq!(
            render(&report(RunOutcome::Completed, "", None), &replies),
            replies.no_answer
        );
    }

    #[test]
    fn an_unfinished_answer_says_why_it_stopped() {
        let replies = Replies::default();
        assert_eq!(
            render(&report(RunOutcome::Truncated, "part", None), &replies),
            format!("part\n\n{}", replies.truncated)
        );
        assert_eq!(
            render(&report(RunOutcome::MaxTurns, "", None), &replies),
            replies.max_turns
        );
    }

    #[test]
    fn a_run_that_did_not_finish_does_not_repeat_an_earlier_answer() {
        let replies = Replies::default();
        assert_eq!(
            render(&report(RunOutcome::Timeout, "earlier", None), &replies),
            replies.timed_out
        );
        assert_eq!(
            render(&report(RunOutcome::Aborted, "earlier", None), &replies),
            replies.cancelled
        );
        assert_eq!(
            render(&report(RunOutcome::Error, "earlier", Some("HTTP 500")), &replies),
            format!("{}HTTP 500", replies.failed)
        );
    }
}
