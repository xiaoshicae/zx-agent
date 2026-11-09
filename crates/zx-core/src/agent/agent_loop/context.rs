//! Context updates and their ordered session records.

use super::{
    AgentEvent, Attempt, AttemptResult, ContextEdit, CustomMessageInfo, EventTarget, Halt, HookFailure, Message,
    Prepared, Queue, QueueMode, QueuedMessage, UserMessageKind, record_pending_entries,
};
use crate::session::CustomMessage;

impl Attempt<'_> {
    /// Record a finished message, then report it. Returns its entry ID.
    pub(super) async fn record_finished_message(&mut self, message: &Message) -> Result<String, Halt> {
        let id = self.session.append_message(message).await.map_err(Halt::Unrecorded)?;
        self.emit(AgentEvent::MessageEnd {
            message: message.clone(),
            entry_id: id.clone(),
            custom: None,
        })
        .await;
        Ok(id)
    }

    pub(super) async fn append(&mut self, message: Message) -> Result<(), Halt> {
        self.append_from(message, None).await
    }

    /// Append `message`, which is the message at the front of a queue if `queued` names one.
    /// It leaves the queue as soon as it is recorded, before observers see it: a run dropped
    /// while they run cannot take the same input again.
    async fn append_from(&mut self, message: Message, queued: Option<(Queue, &QueuedMessage)>) -> Result<(), Halt> {
        self.emit(AgentEvent::MessageStart { role: message.role() }).await;
        let id = self.session.append_message(&message).await.map_err(Halt::Unrecorded)?;
        if let Some((queue, original)) = queued {
            self.config.queues.remove_front(queue, original);
        }
        self.emit(AgentEvent::MessageEnd {
            message,
            entry_id: id,
            custom: None,
        })
        .await;
        Ok(())
    }

    /// Append a message from an extension (pi: a `custom` message): recorded as a
    /// `custom_message` entry, without input hooks, and seen by the model as a user message.
    async fn custom_message(&mut self, message: CustomMessage, queue: Option<Queue>) -> Result<(), Halt> {
        self.emit(AgentEvent::MessageStart { role: "user" }).await;
        let (id, projected) = self
            .session
            .append_custom_message(&message)
            .await
            .map_err(Halt::Unrecorded)?;
        if let Some(queue) = queue {
            self.config
                .queues
                .remove_front(queue, &QueuedMessage::Custom(message.clone()));
        }
        self.emit(AgentEvent::MessageEnd {
            message: projected,
            entry_id: id,
            custom: Some(CustomMessageInfo {
                custom_type: message.custom_type,
                display: message.display,
                details: message.details,
            }),
        })
        .await;
        Ok(())
    }

    /// Record `edit`. Its index refers to the context as earlier edits of the same plan left
    /// it, which is the session's projection by then.
    pub(super) async fn edit(&mut self, edit: ContextEdit) -> Result<(), Halt> {
        let (index, replacement) = match edit {
            ContextEdit::Append { message } => return self.append(message).await,
            ContextEdit::ReplaceText { index, text } => (index, Some(text)),
            ContextEdit::Omit { index } => (index, None),
        };
        let entry_id = self
            .session
            .edit_context(index, replacement.as_deref())
            .await
            .map_err(Halt::Unrecorded)?;
        self.emit(AgentEvent::ContextEdited {
            index,
            replacement,
            entry_id,
        })
        .await;
        Ok(())
    }

    /// Pass user input through the input hooks, then record it. The original text is kept
    /// for auditing: a rewrite or a rejection is recorded as an edit after it. A cancelled
    /// hook decided nothing, so nothing is recorded. Input from `queue` leaves the queue
    /// only once recorded, so a cancelled or dropped run leaves it for the next run.
    pub(super) async fn input(&mut self, message: Message, queue: Option<Queue>) -> Result<(), Halt> {
        let Message::User {
            content,
            kind: UserMessageKind::Prompt,
            ..
        } = &message
        else {
            return self.append(message).await;
        };
        let text = content.clone();
        let transformed = match self.config.hooks.on_input(text.clone(), self.trace.context()).await {
            Err(error) if error.failure == HookFailure::Cancelled => {
                return Err(Halt::Failed(self.report_hook_failure(error)));
            }
            transformed => transformed,
        };
        let queued = QueuedMessage::User(text.clone());
        self.append_from(message, queue.map(|queue| (queue, &queued))).await?;
        // The input is the last message of the projection now.
        let index = self.session.messages().len() - 1;
        match transformed {
            Ok(replacement) if replacement != text => {
                self.edit(ContextEdit::ReplaceText {
                    index,
                    text: replacement,
                })
                .await
            }
            Ok(_) => Ok(()),
            Err(error) => {
                // A rejected input remains in the audit log but is omitted from requests.
                self.edit(ContextEdit::Omit { index }).await?;
                Err(Halt::Failed(self.report_hook_failure(error)))
            }
        }
    }

    /// Record a queued message of either kind; see [`Attempt::input`] and the custom case.
    pub(super) async fn queued_input(&mut self, message: QueuedMessage, queue: Option<Queue>) -> Result<(), Halt> {
        match message {
            QueuedMessage::User(text) => self.input(Message::user(text), queue).await,
            QueuedMessage::Custom(message) => self.custom_message(message, queue).await,
        }
    }

    /// Drain one queue in order, or only its oldest message in one-at-a-time mode; input
    /// removes each item only after it is recorded.
    pub(super) async fn drain_queue(&mut self, queue: Queue) -> Result<(), Halt> {
        let mode = self.config.queues.mode(queue);
        while let Some(message) = self.config.queues.front(queue) {
            self.queued_input(message, Some(queue)).await?;
            if mode == QueueMode::OneAtATime {
                break;
            }
        }
        Ok(())
    }

    /// Record the custom entries extensions asked for; see [`record_pending_entries`].
    pub(super) async fn record_pending_entries(&mut self) -> Result<(), Halt> {
        let target = EventTarget {
            sink: self.sink,
            trace: self.trace.get(),
        };
        record_pending_entries(self.config, &mut *self.session, target)
            .await
            .map_err(Halt::Unrecorded)
    }

    /// Prepare the context and account for any summary requests, even on failure. Returns
    /// how many requests preparing it made.
    pub(super) async fn prepare_context(&mut self, result: &mut AttemptResult) -> Result<u32, Halt> {
        let prepared = self.session.prepare_context(self.trace.cancel()).await;
        result.usage.add(&prepared.usage);
        result.model_requests = result.model_requests.saturating_add(prepared.requests);
        match prepared.result {
            Prepared::Ready => Ok(prepared.requests),
            Prepared::Failed(error) => Err(Halt::Failed(error)),
            Prepared::Unrecorded(error) => Err(Halt::Unrecorded(error)),
        }
    }
}
