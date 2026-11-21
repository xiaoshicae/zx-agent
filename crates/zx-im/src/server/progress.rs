//! What the task that reads a run tells the conversation's worker while the run goes on: the
//! approvals it asks for and how they end, and each answer the run goes on from.

use std::sync::Arc;

use tokio::sync::mpsc;
use zx_agent::ai::{Message, StopReason};
use zx_agent::events::{Event, EventKind, RequestKind};

use super::approval::{ApprovalUpdate, approval_update};
use super::{ServerEvent, ServerSink, SessionKey};

/// What the worker learns while a run goes on, in the order the run reported it: one channel
/// carries all of it, so that an answer is posted before a card that came after it.
pub(super) enum RunUpdate {
    Approval(ApprovalUpdate),
    /// The model finished this answer, and the run went on to what was steered into it
    /// meanwhile: the worker posts it now, as the end of a run says only its last answer.
    Answered(String),
}

/// Passes on to the worker what it shows of a run: its approvals, and each answer the run goes
/// on from.
pub(super) struct RunEvents {
    updates: mpsc::Sender<RunUpdate>,
    key: SessionKey,
    sink: Arc<dyn ServerSink>,
    /// The answer the model finished last, until the run asks the model again or ends.
    answer: Option<String>,
}

impl RunEvents {
    pub(super) fn new(updates: mpsc::Sender<RunUpdate>, key: SessionKey, sink: Arc<dyn ServerSink>) -> Self {
        Self {
            updates,
            key,
            sink,
            answer: None,
        }
    }

    /// Pass on what `event` tells the worker: an approval, or that the run asks the model
    /// again after an answer. A summary request does not count: one made after the last
    /// answer would post that answer twice, once here and once as the run ends.
    pub(super) fn forward(&mut self, event: &Event) {
        if let EventKind::MessageEnd {
            message: Message::Assistant(message),
            ..
        } = &event.event
        {
            // Only an answer the model finished: not what it writes before a tool call, nor
            // a part cut off at the output limit, which the run continues.
            let text = message.text();
            self.answer = (message.stop_reason == StopReason::Stop && !text.trim().is_empty()).then_some(text);
        } else if let EventKind::RequestStart {
            kind: RequestKind::Task,
            ..
        } = &event.event
            && let Some(answer) = self.answer.take()
        {
            self.send(RunUpdate::Answered(answer.trim().to_string()));
        } else if let Some(update) = approval_update(event) {
            self.send(RunUpdate::Approval(update));
        }
    }

    fn send(&self, update: RunUpdate) {
        // Reading the run must not wait for the worker, which may be waiting for the run.
        // The channel holds two updates per approval, asked one at a time, and at most one
        // answer per model request; the worker drains it between its other steps, so it is
        // not expected to fill. If it did, a request would get no card and end when nobody
        // answers it in time, a card would stay open, or an answer would not be posted. A
        // closed channel means the worker has ended: nobody needs the update.
        match self.updates.try_send(update) {
            Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
            Err(mpsc::error::TrySendError::Full(update)) => {
                let error = match update {
                    RunUpdate::Approval(_) => "an approval update was dropped: its card is missing or stays open",
                    RunUpdate::Answered(_) => "an answer the run went on from was dropped: it is not posted",
                };
                self.sink.emit(&ServerEvent::Failed {
                    key: Some(self.key.clone()),
                    error: error.into(),
                });
            }
        }
    }
}
