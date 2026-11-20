//! Where a conversation posts and reports, the session it holds while it serves, and how the
//! model reads what people said there.

use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use zx_agent::events::{RunOutcome, RunReport};
use zx_agent::input::{Input, PendingInput};
use zx_agent::session::SessionError;
use zx_agent::transcript::{CustomMessage, SessionQuery};
use zx_agent::{Agent, Session, SessionOptions, compaction};
use zx_app::shutdown::next_event;

use crate::{Destination, ImChannel, MessageRef, Post, Replacement, UserId};

use super::approval::{ApprovalEvents, ApprovalUpdate};
use super::{Replies, SessionKey, Shared, timed};

/// Messages read from a thread the bot is first mentioned in: enough to follow a discussion,
/// few enough not to crowd the context.
const HISTORY_MESSAGES: usize = 50;
/// The kind of the custom messages that record what people said without the bot answering.
const SAID_TYPE: &str = "im_message";

/// The session while the conversation holds it. Dropping it closes the session, which frees
/// its place in the storage and everything set up for it.
pub(super) struct Opened {
    // Shared with the task that reads the current run, as a Session is meant to be:
    // meanwhile the worker steers, cancels and takes queued input through it.
    pub(super) session: Session,
    /// Keeps the session open until it is closed there.
    agent: Agent,
}

impl Drop for Opened {
    fn drop(&mut self) {
        self.agent.close_session(self.session.id());
    }
}

impl Opened {
    /// Whether the session is still open: not once it was closed while the conversation held
    /// it, as a session closes itself when it can no longer write its records. Asks this
    /// handle, not the agent's table, which may hold the session opened again elsewhere.
    pub(super) fn still_open(&self) -> bool {
        !self.session.is_closed()
    }

    /// What people asked after the run's last look at the queue, which starts the next run.
    /// A message an extension queued goes back to the queue, which that run takes before its
    /// first request. [`SessionError::Closed`] for a session that was closed while the
    /// conversation held it.
    pub(super) fn leftover(&self) -> Result<Vec<Asked>, SessionError> {
        // The loop looks for input at the end of each step, so a message steered after the
        // last look waits in the queue.
        let queued = self.session.take_queued()?;
        let mut asked = Vec::new();
        for message in queued {
            match message.input() {
                Input::User { .. } => asked.push(Asked(message)),
                Input::Custom(_) => self.session.queues().steer(message),
            }
        }
        Ok(asked)
    }

    /// What people asked after the run's last look at the queue, from a session that closed
    /// meanwhile: it takes no more input, and its queues outlive it. A message an extension
    /// queued for that session goes with it.
    pub(super) fn taken_back(&self) -> Vec<Asked> {
        let queues = self.session.queues();
        let queued = queues.take_all();
        queued
            .into_iter()
            .filter_map(|message| match message.input() {
                Input::User { .. } => Some(Asked(message)),
                Input::Custom(_) => None,
            })
            .collect()
    }
}

/// What a person asked the bot: the text as the model reads it, and who sent it.
pub(super) struct Asked(pub(super) PendingInput);

impl Asked {
    /// Several messages as one prompt, which holds what each held until the session records
    /// it; the sender of the last message is whom the answer is for.
    pub(super) fn joined(asked: Vec<Self>) -> PendingInput {
        let mut texts = Vec::with_capacity(asked.len());
        let mut actor = None;
        let mut holds = Vec::with_capacity(asked.len());
        for Asked(pending) in asked {
            let (input, hold) = pending.into_parts();
            if let Input::User { text, actor: sender } = input {
                texts.push(text);
                actor = sender;
            }
            holds.extend(hold);
        }
        let prompt = Input::User {
            text: texts.join("\n\n"),
            actor,
        };
        PendingInput::held(prompt, holds)
    }
}

/// How the task of a running conversation ended: a prompt's run, or `/compact`'s.
pub(super) enum Finished {
    Run(RunReport),
    Compaction(compaction::Report),
}

impl Finished {
    /// Whether it ended because it was cancelled: by `/cancel`, by `/new` or by the stop.
    pub(super) fn cancelled(&self) -> bool {
        match self {
            Self::Run(report) => report.outcome == RunOutcome::Cancelled,
            Self::Compaction(report) => report.outcome == compaction::Outcome::Cancelled,
        }
    }
}

/// Where the conversation posts and reports. Separate from the worker (`worker.rs`) so that
/// posting borrows only these fields while the worker's own state stays with the worker.
pub(super) struct Outlet {
    pub(super) key: SessionKey,
    pub(super) to: Destination,
    pub(super) channel: Arc<dyn ImChannel>,
    pub(super) shared: Arc<Shared>,
}

impl Outlet {
    /// Post text in the conversation. A failure is reported to the operator and returns `None`.
    pub(super) async fn post(&self, text: String) -> Option<MessageRef> {
        let message = Post::Text(text);
        self.shared
            .bounded(Some(&self.key), "post", self.channel.send(&self.to, &message))
            .await
    }

    /// Replace a posted message; false when it was not replaced. A failure is reported to the
    /// operator.
    pub(super) async fn replace(&self, posted: &MessageRef, replacement: &Replacement) -> bool {
        let act = format!("update message {}", posted.id);
        self.shared
            .bounded(Some(&self.key), &act, self.channel.update(posted, replacement))
            .await
            .is_some()
    }

    /// Open the conversation's session, or create its first one. Only that first session,
    /// created for `trigger`, a mention of the bot in a thread someone else started, records
    /// what was said there before, which is read before the session is made: a session either
    /// starts with the thread or is not made. A session the conversation has already, such as
    /// one `/new` started, is opened as it is, however little it holds.
    ///
    /// A session the storage no longer has, such as one whose file was removed, is reported
    /// and replaced as if the conversation were new; any other failure is the caller's to
    /// report, and the next message tries again.
    pub(super) async fn open(&self, trigger: Option<&MessageRef>) -> Result<Opened, String> {
        let shared = &self.shared;
        if let Some(id) = shared.session_of(&self.key) {
            match shared.agent.open_session(&id).await {
                Ok(session) => return Ok(self.hold(session)),
                Err(error) if !self.still_kept(&id).await => {
                    self.failed(format!("session {id} is gone ({error:#}); a new one is made"));
                }
                Err(error) => return Err(format!("{error:#}")),
            }
        }
        let earlier = match trigger {
            Some(trigger) => self.said_before(trigger).await,
            None => Vec::new(),
        };
        let opened = self.create().await?;
        for content in earlier {
            if let Err(error) = opened.session.append_custom_message(said(content)).await {
                self.failed(format!("cannot record the thread: {error}"));
                break;
            }
        }
        Ok(opened)
    }

    /// Whether the storage still has the conversation's session `id`. A storage that cannot
    /// tell counts as having it, so that a passing failure never starts a new session.
    async fn still_kept(&self, id: &str) -> bool {
        let query = SessionQuery {
            labels: self.key.labels(),
            ..SessionQuery::default()
        };
        match self.shared.agent.session_headers(&query).await {
            Ok(headers) => headers.iter().any(|header| header.id == id),
            Err(_) => true,
        }
    }

    /// Create a session with the conversation's labels, which is its latest one from now
    /// on; the earlier ones are kept.
    pub(super) async fn create(&self) -> Result<Opened, String> {
        let options = SessionOptions {
            labels: self.key.labels(),
            ..SessionOptions::new(&self.shared.cwd)
        };
        let created = self.shared.agent.create_session(options).await;
        // The whole chain of causes.
        let session = created.map_err(|error| format!("{error:#}"))?;
        self.shared.go_on_in(&self.key, session.id());
        Ok(self.hold(session))
    }

    /// `session`, as the conversation holds it.
    fn hold(&self, session: Session) -> Opened {
        Opened {
            session,
            agent: self.shared.agent.clone(),
        }
    }

    /// A new session in place of `open`, the one held so far, said in the conversation: the
    /// session the conversation holds from now on. Created at once, it is the conversation's
    /// latest session whatever happens before the next message: a conversation that is
    /// released, or a process that restarts, goes on in it. A session that cannot be created
    /// is reported in the conversation, which then stays in `open`, as it does when `stop`,
    /// the server stopping, ends the wait.
    pub(super) async fn start_over(&self, open: Option<Opened>, stop: &CancellationToken) -> Option<Opened> {
        let created = tokio::select! {
            // The command stays in the backlog, for the next start.
            () = stop.cancelled() => return open,
            created = self.create() => created,
        };
        match created {
            Ok(opened) => {
                // The session held so far is closed here; it stays in the storage.
                drop(open);
                self.post(self.replies().new_session.clone()).await;
                Some(opened)
            }
            Err(error) => {
                self.cannot_open(&error).await;
                open
            }
        }
    }

    /// Tell the conversation, and the operator, that its session could not be opened.
    pub(super) async fn cannot_open(&self, error: &str) {
        self.post(format!("{}{error}", self.replies().cannot_open)).await;
        self.failed(format!("cannot open the session: {error}"));
    }

    /// Post the card of an approval request. An error says why nobody can decide.
    pub(super) async fn post_card(&self, card: &Post) -> Result<MessageRef, String> {
        let post = self.channel.send(&self.to, card);
        timed(self.shared.outbound_timeout, "post the approval request", post).await
    }

    /// Run `prompt` in `session` to its end, as the task of a run does. How its approvals
    /// end goes to the worker through `updates`; `stop`, the server stopping, cancels the
    /// run, which is still read to its end.
    pub(super) fn run(
        &self,
        session: &Session,
        prompt: PendingInput,
        updates: &mpsc::Sender<ApprovalUpdate>,
        stop: CancellationToken,
    ) -> impl Future<Output = Result<Finished, SessionError>> + Send + 'static {
        let session = session.clone();
        let approvals = ApprovalEvents {
            updates: updates.clone(),
            key: self.key.clone(),
            sink: Arc::clone(&self.shared.sink),
        };
        async move {
            let mut run = session.send(prompt)?;
            while let Some(event) = next_event(&mut run, &session, &stop).await {
                approvals.forward(&event);
            }
            Ok(Finished::Run(run.wait().await))
        }
    }

    /// Summarize the earlier part of `session` to the compaction's end, as the task of
    /// `/compact` does; `stop` cancels it as it cancels a run.
    pub(super) fn compact(
        &self,
        session: &Session,
        stop: CancellationToken,
    ) -> impl Future<Output = Result<Finished, SessionError>> + Send + 'static {
        let session = session.clone();
        async move {
            let mut run = session.compact()?;
            while next_event(&mut run, &session, &stop).await.is_some() {}
            Ok(Finished::Compaction(run.wait().await))
        }
    }

    /// Replace the status with `answer`, how the work ended, or post it when no status was
    /// posted or the status could not be replaced.
    pub(super) async fn answer(&self, answer: String, status: Option<MessageRef>) {
        match status {
            Some(posted) => {
                // Forgotten first: should the process end between the two, the status still
                // says the work goes on, as it did before statuses were kept; the other order
                // could have the next start replace this answer.
                self.shared.forget_status(&self.key, &posted).await;
                let replacement = Replacement::Text(answer);
                // Forgotten, the status would say the work goes on for good. An update that
                // failed after the platform took it, as one that timed out may have, shows
                // the answer twice: better than never.
                if !self.replace(&posted, &replacement).await
                    && let Replacement::Text(answer) = replacement
                {
                    self.post(answer).await;
                }
            }
            None => {
                self.post(answer).await;
            }
        }
    }

    pub(super) fn failed(&self, error: String) {
        self.shared.failed(Some(&self.key), error);
    }

    /// `text` as the model reads it: in a group, after its sender's name, since several
    /// people talk there.
    pub(super) async fn attributed(&self, sender: &UserId, text: &str) -> String {
        if self.key.thread.is_none() {
            return text.to_string();
        }
        let name = self.shared.name_of(self.channel.as_ref(), &self.key, sender).await;
        format!("{name}: {text}")
    }

    /// Show the sender that a message reached the run that is answering. A failure is
    /// reported to the operator.
    pub(super) async fn acknowledge(&self, message: &MessageRef) {
        let act = format!("acknowledge message {}", message.id);
        self.shared
            .bounded(Some(&self.key), &act, self.channel.acknowledge(message))
            .await;
    }

    /// What was said in the thread before `trigger`, the mention of the bot that starts the
    /// conversation's first session, as the model reads it: the first answer then knows the
    /// talk it joins. Nothing in a direct chat or a thread that `trigger` starts, and nothing
    /// when the platform cannot give it, which is reported. The thread is read up to now,
    /// which can be well after `trigger` when the session waited for a run slot; what came
    /// after it is cut off, as it reaches the session as messages of its own. When `trigger`
    /// is not among the messages read, all of them are kept.
    async fn said_before(&self, trigger: &MessageRef) -> Vec<String> {
        let Some(thread) = &self.key.thread else {
            return Vec::new();
        };
        if thread.0 == trigger.id.0 {
            return Vec::new();
        }
        let history = self
            .shared
            .bounded(
                Some(&self.key),
                "read the thread",
                self.channel.thread_history(&self.to, HISTORY_MESSAGES),
            )
            .await;
        // A failure was reported; the conversation goes on without the earlier messages.
        let Some(mut history) = history else {
            return Vec::new();
        };
        if let Some(position) = history.iter().position(|said| said.id == trigger.id) {
            history.truncate(position);
        }
        let mut earlier = Vec::new();
        for message in history {
            earlier.push(self.attributed(&message.sender, &message.text).await);
        }
        earlier
    }

    pub(super) fn replies(&self) -> &Replies {
        &self.shared.replies
    }
}

/// What someone said without the bot answering, as the session records it: the model reads
/// it with the next prompt.
pub(super) fn said(content: String) -> CustomMessage {
    CustomMessage {
        custom_type: SAID_TYPE.into(),
        content,
        display: true,
        details: None,
    }
}
