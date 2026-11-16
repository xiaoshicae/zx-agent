//! Context updates and their ordered session records.

use anyhow::{Context, Result, anyhow};

use super::{Halt, StepLoop};
use crate::ai::{Message, UserMessageKind};
use crate::events::{CustomMessageInfo, EventKind};
use crate::hooks::{ContextEdit, HookFailure};
use crate::input::{Input, InputQueues, Queue, QueueMode};
use crate::runner::EventTarget;
use crate::transcript::{CustomMessage, Entry, Transcript};

/// The context of the error for an entry the session could not record.
pub(crate) const PERSIST_FAILED: &str = "cannot persist session entry";

/// Record the custom entries extensions queued since the last step (pi: `appendEntry`), each
/// announced as a `custom_entry` event. An error means the session could not record one.
pub(crate) async fn record_pending_entries(
    queues: &InputQueues,
    transcript: &mut Transcript,
    target: EventTarget<'_>,
) -> Result<()> {
    for entry in queues.take_entries() {
        let entry_id = transcript
            .append_custom_entry(&entry.custom_type, entry.data)
            .await
            .context(PERSIST_FAILED)?;
        target
            .emit(EventKind::CustomEntry {
                entry_id,
                custom_type: entry.custom_type,
            })
            .await;
    }
    Ok(())
}

impl StepLoop<'_> {
    /// Record `message` in the session, with `actor` when a person the host names sent it.
    /// Returns its entry ID.
    async fn append_message(&mut self, message: &Message, actor: Option<String>) -> Result<String, Halt> {
        // The event that reports the message owns it, so the session stores a copy.
        self.host
            .transcript
            .append_message_from(message.clone(), actor)
            .await
            .context(PERSIST_FAILED)
            .map_err(Halt::Unrecorded)
    }

    /// Record a finished message, then report it. Returns its entry ID.
    pub(super) async fn record_finished_message(&mut self, message: &Message) -> Result<String, Halt> {
        let id = self.append_message(message, None).await?;
        self.emit(EventKind::MessageEnd {
            message: message.clone(),
            entry_id: id.clone(),
            custom: None,
        })
        .await;
        Ok(id)
    }

    /// Append a message that arrives whole: report its start, record it, then report its end.
    pub(super) async fn append(&mut self, message: Message) -> Result<String, Halt> {
        self.append_from(message, None, None).await
    }

    /// Append `message`, which `actor` sent when it is a person's input, and which is the
    /// message at the front of a queue if `queued` names one. It leaves the queue as soon as
    /// it is recorded, before observers see it: a run dropped while they run cannot take the
    /// same input again.
    async fn append_from(
        &mut self,
        message: Message,
        actor: Option<String>,
        queued: Option<(Queue, &Input)>,
    ) -> Result<String, Halt> {
        self.emit(EventKind::MessageStart { role: message.role() }).await;
        let id = self.append_message(&message, actor).await?;
        if let Some((queue, original)) = queued {
            self.host.config.queues.remove_front(queue, original);
        }
        self.emit(EventKind::MessageEnd {
            message,
            entry_id: id.clone(),
            custom: None,
        })
        .await;
        Ok(id)
    }

    /// Append a message from an extension (pi: a `custom` message): recorded as a
    /// `custom_message` entry, without input hooks, and seen by the model as a user message.
    async fn custom_message(&mut self, message: CustomMessage, queue: Option<Queue>) -> Result<(), Halt> {
        self.emit(EventKind::MessageStart { role: "user" }).await;
        let transcript = &mut *self.host.transcript;
        let id = transcript
            .append_custom_message(message.clone())
            .await
            .context(PERSIST_FAILED)
            .map_err(Halt::Unrecorded)?;
        let projected = match transcript.get(&id) {
            Some(Entry::CustomMessage(entry)) => entry.projected(),
            _ => return Err(Halt::Unrecorded(anyhow!("custom message {id} was not recorded"))),
        };
        if let Some(queue) = queue {
            self.host
                .config
                .queues
                .remove_front(queue, &Input::Custom(message.clone()));
        }
        self.emit(EventKind::MessageEnd {
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

    /// Record `edits`, the plan of the context hooks, made over a context whose messages are
    /// the entries `targets`. An index names a message of that context as the plan's earlier
    /// edits left it, which the session's projection may no longer show once an edit ends a
    /// summary: so the targets follow the plan, and a message it appends joins them once
    /// recorded. The chain applied the plan to a copy of the same context, so every index is
    /// in bounds.
    pub(super) async fn record_plan(&mut self, mut targets: Vec<String>, edits: Vec<ContextEdit>) -> Result<(), Halt> {
        let out_of_bounds = |index: usize| Halt::Failed(format!("context index {index} is out of bounds"));
        for edit in edits {
            match edit {
                ContextEdit::Append { message } => targets.push(self.append(message).await?),
                ContextEdit::ReplaceText { index, text } => {
                    let target = targets.get(index).cloned().ok_or_else(|| out_of_bounds(index))?;
                    self.edit_entry(target, index, Some(text)).await?;
                }
                ContextEdit::Omit { index } => {
                    if index >= targets.len() {
                        return Err(out_of_bounds(index));
                    }
                    let target = targets.remove(index);
                    self.edit_entry(target, index, None).await?;
                }
            }
        }
        Ok(())
    }

    /// Record an edit of the entry `target`, at `index` of the context the edit was planned
    /// over: its text replaced by `replacement`, or, with `None`, the message omitted.
    async fn edit_entry(&mut self, target: String, index: usize, replacement: Option<String>) -> Result<(), Halt> {
        let entry_id = self
            .host
            .transcript
            .append_context_edit(&target, replacement.clone())
            .await
            .context("cannot persist context edit")
            .map_err(Halt::Unrecorded)?;
        self.emit(EventKind::ContextEdited {
            index,
            replacement,
            entry_id,
        })
        .await;
        Ok(())
    }

    /// Pass user input through the input hooks, then record it, with `actor` when a person
    /// the host names sent it. The original text is kept for auditing: a rewrite or a
    /// rejection is recorded as an edit after it. A cancelled hook decided nothing, so
    /// nothing is recorded. Input from `queue` leaves the queue only once recorded, so a
    /// cancelled or dropped run leaves it for the next run.
    ///
    /// From a prompt on, the hooks and the policies are told that the run answers `actor`.
    pub(super) async fn input(
        &mut self,
        message: Message,
        actor: Option<String>,
        queue: Option<Queue>,
    ) -> Result<(), Halt> {
        let Message::User {
            content,
            kind: UserMessageKind::Prompt,
            ..
        } = &message
        else {
            return self.append(message).await.map(drop);
        };
        self.host.target.trace.answer(actor.as_deref());
        let text = content.clone();
        let transformed = match self
            .host
            .config
            .hooks
            .on_input(text.clone(), &self.host.target.trace.context())
            .await
        {
            Err(error) if error.failure == HookFailure::Cancelled => {
                return Err(self.hook_failed(error).await);
            }
            transformed => transformed,
        };
        let queued = Input::User {
            text: text.clone(),
            actor: actor.clone(),
        };
        let id = self
            .append_from(message, actor, queue.map(|queue| (queue, &queued)))
            .await?;
        // The input is the last message of the projection now.
        let index = self.host.transcript.messages().len() - 1;
        match transformed {
            Ok(replacement) if replacement != text => self.edit_entry(id, index, Some(replacement)).await,
            Ok(_) => Ok(()),
            Err(error) => {
                // A rejected input remains in the audit log but is omitted from requests.
                self.edit_entry(id, index, None).await?;
                Err(self.hook_failed(error).await)
            }
        }
    }

    /// Record a queued message of either kind; see [`StepLoop::input`] and the custom case.
    pub(super) async fn queued_input(&mut self, message: Input, queue: Option<Queue>) -> Result<(), Halt> {
        match message {
            Input::User { text, actor } => self.input(Message::user(text), actor, queue).await,
            Input::Custom(message) => self.custom_message(message, queue).await,
        }
    }

    /// Drain one queue in order, or only its oldest message in one-at-a-time mode; input
    /// removes each item only after it is recorded.
    pub(super) async fn drain_queue(&mut self, queue: Queue) -> Result<(), Halt> {
        let mode = self.host.config.queues.mode();
        while let Some(message) = self.host.config.queues.front(queue) {
            self.queued_input(message, Some(queue)).await?;
            if mode == QueueMode::OneAtATime {
                break;
            }
        }
        Ok(())
    }

    /// Record the custom entries extensions asked for; see [`record_pending_entries`].
    pub(super) async fn record_pending_entries(&mut self) -> Result<(), Halt> {
        let target = self.target();
        record_pending_entries(&self.host.config.queues, self.host.transcript, target)
            .await
            .map_err(Halt::Unrecorded)
    }
}
