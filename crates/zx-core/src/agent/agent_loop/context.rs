//! Context updates and their ordered session records.

use super::{
    AgentEvent, ContextEdit, ContextPreparation, CustomMessageInfo, Engine, Halt, HookFailure, LoopResult, Message,
    Prepared, Queue, QueueMode, QueuedMessage, UserMessageKind, now_ms,
};
use crate::session::CustomMessage;

impl Engine<'_, '_, '_> {
    /// Record a finished message. Returns its entry ID in a session.
    pub(super) async fn record(&mut self, message: &Message) -> Result<Option<String>, Halt> {
        match self.control.session.as_deref_mut() {
            Some(session) => Ok(Some(session.append_message(message).await.map_err(Halt::Unrecorded)?)),
            None => Ok(None),
        }
    }

    /// Record a finished message, then report it. Returns its entry ID in a session.
    pub(super) async fn record_finished_message(&mut self, message: &Message) -> Result<Option<String>, Halt> {
        let id = self.record(message).await?;
        self.emit(AgentEvent::MessageEnd {
            message: message.clone(),
            entry_id: id.clone(),
            custom: None,
        })
        .await;
        Ok(id)
    }

    pub(super) async fn append(&mut self, context: &mut Vec<Message>, message: Message) -> Result<(), Halt> {
        self.append_from(context, message, None).await
    }

    /// Append `message`, which is the message at the front of a queue if `queued` names one.
    /// It leaves the queue as soon as it is recorded, before observers see it: a run dropped
    /// while they run cannot take the same input again.
    pub(super) async fn append_from(
        &mut self,
        context: &mut Vec<Message>,
        message: Message,
        queued: Option<(Queue, &QueuedMessage)>,
    ) -> Result<(), Halt> {
        self.emit(AgentEvent::MessageStart { role: message.role() }).await;
        let id = self.record(&message).await?;
        if let Some((queue, original)) = queued {
            self.config.queues.remove_front(queue, original);
        }
        self.emit(AgentEvent::MessageEnd {
            message: message.clone(),
            entry_id: id,
            custom: None,
        })
        .await;
        context.push(message);
        Ok(())
    }

    /// Append a message from an extension (pi: a `custom` message): recorded as a
    /// `custom_message` entry, without input hooks, and seen by the model as a user message.
    async fn custom_message(
        &mut self,
        context: &mut Vec<Message>,
        message: CustomMessage,
        queue: Option<Queue>,
    ) -> Result<(), Halt> {
        self.emit(AgentEvent::MessageStart { role: "user" }).await;
        let (id, projected) = match self.control.session.as_deref_mut() {
            Some(session) => {
                let (id, projected) = session
                    .append_custom_message(&message)
                    .await
                    .map_err(Halt::Unrecorded)?;
                (Some(id), projected)
            }
            None => (
                None,
                Message::User {
                    content: message.content.clone(),
                    timestamp: now_ms(),
                    kind: UserMessageKind::Custom,
                },
            ),
        };
        if let Some(queue) = queue {
            self.config
                .queues
                .remove_front(queue, &QueuedMessage::Custom(message.clone()));
        }
        self.emit(AgentEvent::MessageEnd {
            message: projected.clone(),
            entry_id: id,
            custom: Some(CustomMessageInfo {
                custom_type: message.custom_type,
                display: message.display,
                details: message.details,
            }),
        })
        .await;
        context.push(projected);
        Ok(())
    }

    pub(super) async fn edit(&mut self, context: &mut Vec<Message>, edit: ContextEdit) -> Result<(), Halt> {
        let (index, replacement) = match &edit {
            ContextEdit::Append { message } => return self.append(context, message.clone()).await,
            ContextEdit::ReplaceText { index, text } => (*index, Some(text.clone())),
            ContextEdit::Omit { index } => (*index, None),
        };
        let entry_id = match self.control.session.as_deref_mut() {
            Some(session) => Some(
                session
                    .edit_context(index, replacement.as_deref())
                    .await
                    .map_err(Halt::Unrecorded)?,
            ),
            None => None,
        };
        edit.apply(context).map_err(Halt::Failed)?;
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
    pub(super) async fn input(
        &mut self,
        context: &mut Vec<Message>,
        message: Message,
        queue: Option<Queue>,
    ) -> Result<(), Halt> {
        let Message::User {
            content,
            kind: UserMessageKind::Prompt,
            ..
        } = &message
        else {
            return self.append(context, message).await;
        };
        let text = content.clone();
        let transformed = match self.config.hooks.on_input(text.clone(), self.trace.context()).await {
            Err(error) if error.failure == HookFailure::Cancelled => return Err(Halt::Failed(self.hook_error(error))),
            transformed => transformed,
        };
        let queued = QueuedMessage::User(text.clone());
        self.append_from(context, message, queue.map(|queue| (queue, &queued)))
            .await?;
        let index = context.len() - 1;
        match transformed {
            Ok(replacement) if replacement != text => {
                self.edit(
                    context,
                    ContextEdit::ReplaceText {
                        index,
                        text: replacement,
                    },
                )
                .await
            }
            Ok(_) => Ok(()),
            Err(error) => {
                // A rejected input remains in the audit log but is omitted from requests.
                self.edit(context, ContextEdit::Omit { index }).await?;
                Err(Halt::Failed(self.hook_error(error)))
            }
        }
    }

    /// Record a queued message of either kind; see [`Engine::input`] and the custom case.
    pub(super) async fn queued_input(
        &mut self,
        context: &mut Vec<Message>,
        message: QueuedMessage,
        queue: Option<Queue>,
    ) -> Result<(), Halt> {
        match message {
            QueuedMessage::User(text) => self.input(context, Message::user(text), queue).await,
            QueuedMessage::Custom(message) => self.custom_message(context, message, queue).await,
        }
    }

    /// Drain one queue in order, or only its oldest message in one-at-a-time mode; input
    /// removes each item only after it is recorded.
    pub(super) async fn drain_queue(&mut self, context: &mut Vec<Message>, queue: Queue) -> Result<(), Halt> {
        let mode = self.config.queues.mode(queue);
        while let Some(message) = self.config.queues.front(queue) {
            self.queued_input(context, message, Some(queue)).await?;
            if mode == QueueMode::OneAtATime {
                break;
            }
        }
        Ok(())
    }

    /// Record the custom entries extensions asked for since the last turn (pi:
    /// `appendEntry`). Without a session they are reported without an entry ID and not kept.
    pub(super) async fn record_pending_entries(&mut self) -> Result<(), Halt> {
        for entry in self.config.queues.take_entries() {
            let entry_id = match self.control.session.as_deref_mut() {
                Some(session) => Some(
                    session
                        .append_custom_entry(&entry.custom_type, entry.data)
                        .await
                        .map_err(Halt::Unrecorded)?,
                ),
                None => None,
            };
            self.emit(AgentEvent::CustomEntry {
                entry_id,
                custom_type: entry.custom_type,
            })
            .await;
        }
        Ok(())
    }

    /// Prepare the context and account for any summary requests, even on failure.
    pub(super) async fn prepare_context(
        &mut self,
        context: &mut Vec<Message>,
        result: &mut LoopResult,
    ) -> Result<u32, Halt> {
        let prepared = match self.control.session.as_deref_mut() {
            Some(session) => session.prepare_context(self.control.cancel).await,
            None => ContextPreparation::unchanged(),
        };
        result.usage.add(&prepared.usage);
        result.model_requests = result.model_requests.saturating_add(prepared.requests);
        match prepared.result {
            Prepared::Unchanged => {}
            Prepared::Updated(messages) => *context = messages,
            Prepared::Failed(error) => return Err(Halt::Failed(error)),
            Prepared::Unrecorded(error) => return Err(Halt::Unrecorded(error)),
        }
        Ok(prepared.requests)
    }
}
