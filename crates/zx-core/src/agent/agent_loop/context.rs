//! Context updates and their ordered session records.

use super::{
    AgentEvent, AgentLoop, Context, ContextEdit, CustomMessageInfo, Entry, EventTarget, Halt, HookFailure, LoopResult,
    Message, PERSIST_FAILED, Queue, QueueMode, QueuedMessage, UserMessageKind, anyhow, record_pending_entries,
};
use crate::session::CustomMessage;

impl AgentLoop<'_> {
    /// Record `message` in the session. Returns its entry ID.
    async fn append_message(&mut self, message: &Message) -> Result<String, Halt> {
        // The event that reports the message owns it, so the session stores a copy.
        self.host
            .session()
            .append_message(message.clone())
            .await
            .context(PERSIST_FAILED)
            .map_err(Halt::Unrecorded)
    }

    /// Record a finished message, then report it. Returns its entry ID.
    pub(super) async fn record_finished_message(&mut self, message: &Message) -> Result<String, Halt> {
        let id = self.append_message(message).await?;
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
        let id = self.append_message(&message).await?;
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
        let session = self.host.session();
        let id = session
            .append_custom_message(message.clone())
            .await
            .context(PERSIST_FAILED)
            .map_err(Halt::Unrecorded)?;
        let projected = match session.get(&id) {
            Some(Entry::CustomMessage(entry)) => entry.projected(),
            _ => return Err(Halt::Unrecorded(anyhow!("custom message {id} was not recorded"))),
        };
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
        let session = self.host.session();
        let target = session
            .projection()
            .get(index)
            .map(|target| target.entry_id.clone())
            .ok_or_else(|| Halt::Unrecorded(anyhow!("context index {index} is out of bounds")))?;
        let entry_id = session
            .append_context_edit(&target, replacement.clone())
            .await
            .context("cannot persist context edit")
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
        let index = self.host.session().messages().len() - 1;
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

    /// Record a queued message of either kind; see [`AgentLoop::input`] and the custom case.
    pub(super) async fn queued_input(&mut self, message: QueuedMessage, queue: Option<Queue>) -> Result<(), Halt> {
        match message {
            QueuedMessage::User(text) => self.input(Message::user(text), queue).await,
            QueuedMessage::Custom(message) => self.custom_message(message, queue).await,
        }
    }

    /// Drain one queue in order, or only its oldest message in one-at-a-time mode; input
    /// removes each item only after it is recorded.
    pub(super) async fn drain_queue(&mut self, queue: Queue) -> Result<(), Halt> {
        let mode = self.config.queues.mode();
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
            trace: self.trace,
        };
        record_pending_entries(self.config, self.host.session(), target)
            .await
            .map_err(Halt::Unrecorded)
    }

    /// Prepare the context and account for any summary requests, even on failure. Returns
    /// how many requests preparing it made.
    pub(super) async fn prepare_context(&mut self, result: &mut LoopResult) -> Result<u32, Halt> {
        let prepared = self.host.prepare_context(self.trace.cancel()).await;
        result.usage.add(&prepared.usage);
        result.model_requests = result.model_requests.saturating_add(prepared.requests);
        prepared.result.map(|()| prepared.requests)
    }
}
