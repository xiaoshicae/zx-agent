//! Where a conversation posts and reports, the session it holds while it serves, how the
//! model reads what people said there, and the text that replaces the status message when a
//! run ends.

use std::fmt::Display;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;

use crate::session::SessionError;
use crate::transcript::CustomMessage;
use crate::{Agent, Session};
use zx_core::SessionOptions;
use zx_core::events::{Budget, RunOutcome, RunReport};
use zx_core::input::{Input, PendingInput};

use super::approval::{ApprovalEvents, ApprovalUpdate};
use super::server::{Shared, timed};
use super::{Destination, ImChannel, MessageRef, Post, Replacement, Replies, SessionKey, UserId};

/// Messages read from a thread the bot is first mentioned in: enough to follow a discussion,
/// few enough not to crowd the context.
const HISTORY_MESSAGES: usize = 50;
/// The kind of the custom messages that record what people said without the bot answering.
const SAID_TYPE: &str = "im_message";

/// The session while the conversation holds it. Dropping it closes the session, which frees
/// its place in the storage and everything set up for it.
pub(crate) struct Opened {
    // Shared with the task that reads the current run, as a Session is meant to be:
    // meanwhile the worker steers, cancels and takes queued input through it.
    pub(crate) session: Session,
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
    pub(crate) fn still_open(&self) -> bool {
        !self.session.is_closed()
    }

    /// What people asked after the run's last look at the queue, which starts the next run.
    /// A message an extension queued goes back to the queue, which that run takes before its
    /// first request. [`SessionError::Closed`] for a session that was closed while the
    /// conversation held it.
    pub(crate) fn leftover(&self) -> Result<Vec<Asked>, SessionError> {
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
    pub(crate) fn taken_back(&self) -> Vec<Asked> {
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
pub(crate) struct Asked(pub(crate) PendingInput);

impl Asked {
    /// Several messages as one running prompt. Their reservations are released as they
    /// leave pending queues; the sender of the last message is whom the answer is for.
    pub(crate) fn joined(asked: Vec<Self>) -> Input {
        let mut texts = Vec::with_capacity(asked.len());
        let mut actor = None;
        for Asked(pending) in asked {
            if let Input::User { text, actor: sender } = pending.into_input() {
                texts.push(text);
                actor = sender;
            }
        }
        Input::User {
            text: texts.join("\n\n"),
            actor,
        }
    }
}

/// Where the conversation posts and reports. Separate from the worker (`worker.rs`) so that
/// posting borrows only these fields while the worker's own state stays with the worker.
pub(crate) struct Outlet {
    pub(crate) key: SessionKey,
    pub(crate) to: Destination,
    pub(crate) channel: Arc<dyn ImChannel>,
    pub(crate) shared: Arc<Shared>,
}

impl Outlet {
    /// Post text in the conversation. A failure is reported to the operator and returns `None`.
    pub(crate) async fn post(&self, text: String) -> Option<MessageRef> {
        let message = Post::Text(text);
        self.shared
            .bounded(Some(&self.key), "post", self.channel.send(&self.to, &message))
            .await
    }

    /// Replace a posted message. A failure is reported to the operator.
    pub(crate) async fn replace(&self, posted: &MessageRef, replacement: Replacement) {
        let act = format!("update message {}", posted.id);
        self.shared
            .bounded(Some(&self.key), &act, self.channel.update(posted, &replacement))
            .await;
    }

    /// Open the conversation's latest session, or create its first one. Only that first
    /// session, created for `trigger`, a mention of the bot in a thread someone else started,
    /// records what was said there before: a session the conversation has already, such as
    /// one `/new` started, is opened as it is, however little it holds.
    pub(crate) async fn open(&self, trigger: Option<&MessageRef>) -> Result<Opened, String> {
        let shared = &self.shared;
        if let Some(id) = shared.session_of(&self.key) {
            return self.held(shared.agent.open_session(&id).await);
        }
        let opened = self.create().await?;
        if let Some(trigger) = trigger {
            self.read_thread(&opened, trigger).await;
        }
        Ok(opened)
    }

    /// Create a session with the conversation's labels, which is its latest one from now
    /// on; the earlier ones are kept.
    pub(crate) async fn create(&self) -> Result<Opened, String> {
        let options = SessionOptions {
            labels: self.key.labels(),
            ..SessionOptions::new(&self.shared.cwd)
        };
        let opened = self.held(self.shared.agent.create_session(options).await)?;
        self.shared.go_on_in(&self.key, opened.session.id());
        Ok(opened)
    }

    /// `session`, as the conversation holds it, or why the agent could not give one: the
    /// whole chain of causes.
    fn held<E: Display>(&self, session: Result<Session, E>) -> Result<Opened, String> {
        match session {
            Ok(session) => Ok(Opened {
                session,
                agent: self.shared.agent.clone(),
            }),
            Err(error) => Err(format!("{error:#}")),
        }
    }

    /// Tell the conversation, and the operator, that its session could not be opened.
    pub(crate) async fn cannot_open(&self, error: &str) {
        self.post(format!("{}{error}", self.replies().cannot_open)).await;
        self.failed(format!("cannot open the session: {error}"));
    }

    /// Post the card of an approval request. An error says why nobody can decide.
    pub(crate) async fn ask(&self, card: &Post) -> Result<MessageRef, String> {
        let post = self.channel.send(&self.to, card);
        timed(self.shared.outbound_timeout, "post the approval request", post).await
    }

    /// Run `prompt` in `session` to its end, as the task of a run does. How its approvals
    /// end goes to the worker through `updates`; `stop`, the server stopping, cancels the
    /// run, which is still read to its end.
    pub(crate) fn run(
        &self,
        session: &Session,
        prompt: Input,
        updates: &mpsc::Sender<ApprovalUpdate>,
        stop: CancellationToken,
    ) -> impl Future<Output = Result<RunReport, SessionError>> + Send + 'static {
        let session = session.clone();
        let approvals = ApprovalEvents {
            updates: updates.clone(),
            key: self.key.clone(),
            sink: Arc::clone(&self.shared.sink),
        };
        async move {
            let mut run = session.send(prompt)?;
            let mut stopped = false;
            loop {
                tokio::select! {
                    () = stop.cancelled(), if !stopped => {
                        session.cancel();
                        stopped = true;
                    }
                    event = run.next() => match event {
                        Some(event) => approvals.forward(&event),
                        None => break,
                    },
                }
            }
            Ok(run.wait().await)
        }
    }

    /// Replace the status with how the run ended, or post that when no status was posted.
    pub(crate) async fn answer(
        &self,
        ended: Result<Result<RunReport, SessionError>, JoinError>,
        status: Option<MessageRef>,
        closing: bool,
    ) {
        let replies = self.replies();
        let answer = match ended {
            // The stop cancels runs through their token. A `/cancel` just before the stop ends
            // the same way and is told the same.
            Ok(Ok(report)) if closing && matches!(report.outcome, RunOutcome::Cancelled) => replies.stopped.clone(),
            Ok(Ok(report)) => render(&report, replies),
            // Only this worker prompts the session, one run at a time, so Busy cannot occur.
            Ok(Err(error)) => format!("{}{error}", replies.failed),
            Err(error) => format!("{}{error}", replies.failed),
        };
        match status {
            Some(posted) => {
                // Forgotten first: should the process end between the two, the status still
                // says the work goes on, as it did before statuses were kept; the other order
                // could have the next start replace this answer.
                self.shared.forget_status(&self.key, &posted).await;
                self.replace(&posted, Replacement::Text(answer)).await;
            }
            None => {
                self.post(answer).await;
            }
        }
    }

    pub(crate) fn failed(&self, error: String) {
        self.shared.failed(Some(&self.key), error);
    }

    /// `text` as the model reads it: in a group, after its sender's name, since several
    /// people talk there.
    pub(crate) async fn attributed(&self, sender: &UserId, text: &str) -> String {
        if self.key.thread.is_none() {
            return text.to_string();
        }
        let name = self.shared.name_of(self.channel.as_ref(), &self.key, sender).await;
        format!("{name}: {text}")
    }

    /// Show the sender that a message reached the run that is answering. A failure is
    /// reported to the operator.
    pub(crate) async fn acknowledge(&self, message: &MessageRef) {
        let act = format!("acknowledge message {}", message.id);
        self.shared
            .bounded(Some(&self.key), &act, self.channel.acknowledge(message))
            .await;
    }

    /// Record what was said in the thread before the bot was mentioned there by `trigger`, in
    /// the conversation's first session, which has nothing yet: the first answer then knows
    /// the talk it joins. Nothing is read when `trigger` started the thread.
    async fn read_thread(&self, opened: &Opened, trigger: &MessageRef) {
        let Some(thread) = &self.key.thread else {
            return;
        };
        if thread.0 == trigger.id.0 {
            return;
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
        let Some(history) = history else {
            return;
        };
        for earlier in history.into_iter().filter(|earlier| earlier.id != trigger.id) {
            let content = self.attributed(&earlier.sender, &earlier.text).await;
            if let Err(error) = opened.session.append_custom_message(said(content)).await {
                self.failed(format!("cannot record the thread: {error}"));
                return;
            }
        }
    }

    pub(crate) fn replies(&self) -> &Replies {
        &self.shared.replies
    }
}

/// What someone said without the bot answering, as the session records it: the model reads
/// it with the next prompt.
pub(crate) fn said(content: String) -> CustomMessage {
    CustomMessage {
        custom_type: SAID_TYPE.into(),
        content,
        display: true,
        details: None,
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
        RunOutcome::BudgetExhausted { budget, truncated } => {
            let reached = match budget {
                Budget::ModelRequests => &replies.model_request_limit,
                Budget::Tokens => &replies.token_budget,
                Budget::Cost => &replies.cost_budget,
            };
            if truncated {
                noted(&format!("{}\n{reached}", replies.truncated))
            } else {
                noted(reached)
            }
        }
        // The last answer in the session may be an earlier run's, so it is not repeated.
        RunOutcome::TimedOut => replies.timed_out.clone(),
        RunOutcome::Cancelled => replies.cancelled.clone(),
        RunOutcome::Failed => match &report.error {
            Some(error) => format!("{}{error}", replies.failed),
            // The reply leads into the error; without one, drop the separator it ends with,
            // whatever the reply's language.
            None => replies
                .failed
                .trim_end_matches(|c: char| c.is_whitespace() || c == ':' || c == '\u{ff1a}')
                .to_string(),
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
            model_requests: 1,
            retries: 0,
            usage: Usage::default(),
            tool_usage: std::collections::BTreeMap::new(),
            denied_calls: 0,
            error: error.map(String::from),
            session_id: "session".into(),
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
        let exhausted = |budget, truncated| RunOutcome::BudgetExhausted { budget, truncated };
        assert_eq!(
            render(&report(exhausted(Budget::ModelRequests, false), "", None), &replies),
            replies.model_request_limit
        );
        assert_eq!(
            render(&report(exhausted(Budget::Tokens, false), "part", None), &replies),
            format!("part\n\n{}", replies.token_budget)
        );
        // Cut off as well: both are said.
        assert_eq!(
            render(&report(exhausted(Budget::Cost, true), "part", None), &replies),
            format!("part\n\n{}\n{}", replies.truncated, replies.cost_budget)
        );
    }

    #[test]
    fn a_run_that_did_not_finish_does_not_repeat_an_earlier_answer() {
        let replies = Replies::default();
        assert_eq!(
            render(&report(RunOutcome::TimedOut, "earlier", None), &replies),
            replies.timed_out
        );
        assert_eq!(
            render(&report(RunOutcome::Cancelled, "earlier", None), &replies),
            replies.cancelled
        );
        assert_eq!(
            render(&report(RunOutcome::Failed, "earlier", Some("HTTP 500")), &replies),
            format!("{}HTTP 500", replies.failed)
        );
    }
}
