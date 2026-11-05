//! Context updates and their ordered session records.

use super::{
    AgentEvent, ContextEdit, ContextPreparation, Engine, Halt, HookFailure, LoopResult, Message, Queue, Usage,
    UserMessageKind,
};

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
        })
        .await;
        Ok(id)
    }

    pub(super) async fn append(&mut self, context: &mut Vec<Message>, message: Message) -> Result<(), Halt> {
        self.append_from(context, message, None).await
    }

    /// Append `message`, which is the text at the front of a queue if `queued` names one. It
    /// leaves the queue as soon as it is recorded, before observers see it: a run dropped
    /// while they run cannot take the same input again.
    pub(super) async fn append_from(
        &mut self,
        context: &mut Vec<Message>,
        message: Message,
        queued: Option<(Queue, &str)>,
    ) -> Result<(), Halt> {
        self.emit(AgentEvent::MessageStart { role: message.role() }).await;
        let id = self.record(&message).await?;
        if let Some((queue, text)) = queued {
            self.config.queues.remove_front(queue, text);
        }
        self.emit(AgentEvent::MessageEnd {
            message: message.clone(),
            entry_id: id,
        })
        .await;
        context.push(message);
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
        self.append_from(context, message, queue.map(|queue| (queue, text.as_str())))
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

    /// Drain one queue in order; input removes each item only after it is recorded.
    pub(super) async fn drain_queue(&mut self, context: &mut Vec<Message>, queue: Queue) -> Result<(), Halt> {
        while let Some(text) = self.config.queues.front(queue) {
            self.input(context, Message::user(text), Some(queue)).await?;
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
            None => ContextPreparation::Unchanged,
        };
        let (requests, usage, failure) = match prepared {
            ContextPreparation::Unchanged => (0, Usage::default(), None),
            ContextPreparation::Updated {
                messages,
                requests,
                usage,
            } => {
                *context = messages;
                (requests, usage, None)
            }
            ContextPreparation::Failed { error, requests, usage } => (requests, usage, Some(Halt::Failed(error))),
            ContextPreparation::Unrecorded { error, requests, usage } => {
                (requests, usage, Some(Halt::Unrecorded(error)))
            }
        };
        result.usage.add(&usage);
        result.model_requests = result.model_requests.saturating_add(requests);
        if let Some(halt) = failure {
            return Err(halt);
        }
        Ok(requests)
    }
}
