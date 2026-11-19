//! The scope of a run: what it borrows from the Runner (the session it records in, the
//! configuration with the model it summarizes with, the compaction settings and pause) with
//! what is its own (where its events go, and where it started), and what it does across its
//! attempts with them. The Runner builds one per run, and each attempt's loop records through
//! it: recording goes to the session as it would without a run, and preparing the context may
//! summarize it (pi: AgentSession writes on `message_end`).

use anyhow::Context;
use tokio_util::sync::CancellationToken;

use super::accounting::check_priced;
use super::{Ending, EventTarget, FirstInput, Halt, LoopConfig, RunAccounting, Start};
use crate::ai::{Message, StopReason};
use crate::compaction::{self, Compacted, Compactor, End, SummaryReply, Trigger};
use crate::events::EventKind;
use crate::input::{Input, Queue};
use crate::transcript::Transcript;

/// The context of the error for an entry the session could not record.
pub(super) const PERSIST_FAILED: &str = "cannot persist session entry";

/// How a run ends: as its last attempt ended, or failed for a reason.
pub(super) enum RunStop {
    /// The last attempt ended this way, which explains itself.
    Ended(Ending),
    /// The run failed, for this reason.
    Failed(String),
}

/// The last message of the conversation the model reads that is not a system message.
enum LastMessage {
    /// None: the conversation is empty, or holds only system messages.
    Nothing,
    /// A prompt, a message or tool results, which the model is to answer.
    Unanswered,
    /// An answer that failed or was cancelled, by its entry.
    Failed(String),
    /// A finished answer.
    Answered,
}

/// Why automatic compaction is paused: a paid summary request failed, and repeating it on
/// every prompt would keep paying for the same failure. A compaction that succeeds, a manual
/// one included, or a change of model resumes it. The Runner owns it, so the pause
/// outlives one prompt and a run whose future is dropped.
#[derive(Default)]
pub(super) struct CompactionPause(Option<String>);

impl CompactionPause {
    /// Why automatic compaction does not run now, when it is paused.
    pub(super) fn refusal(&self) -> Option<String> {
        let reason = self.0.as_ref()?;
        Some(format!(
            "automatic compaction is paused because it failed earlier in this session ({reason})"
        ))
    }

    pub(super) fn clear(&mut self) {
        self.0 = None;
    }

    /// Pause after a failure the summary request was answered for, which asking again would
    /// pay for again; resume after a success. Other ends change nothing: a request that got
    /// no answer may succeed the next time.
    fn note(&mut self, compacted: &Compacted) {
        let answered = compacted.reply == SummaryReply::Answered;
        match &compacted.end {
            End::Completed { .. } => self.0 = None,
            End::Failed(error) if answered => self.0 = Some(error.clone()),
            End::Unrecorded(error) if answered => self.0 = Some(format!("{error:#}")),
            End::Failed(_) | End::Unrecorded(_) | End::Noop | End::Cancelled(_) => {}
        }
    }
}

/// Everything one run works with: its borrow of the Runner (the session, the configuration
/// with its hooks and model, the compaction settings and pause) and its own event target,
/// start and cost budget. The loop of each attempt and the compactor work through it.
pub(super) struct RunScope<'a> {
    pub(super) transcript: &'a mut Transcript,
    /// How many entries the session had when the run started: what follows is the run's.
    pub(super) run_start: usize,
    pub(super) target: EventTarget<'a>,
    pub(super) config: &'a LoopConfig,
    pub(super) compaction: &'a compaction::Config,
    pub(super) pause: &'a mut CompactionPause,
    /// The run's cost budget, which every summary request must be priced for.
    pub(super) cost_budget: Option<f64>,
}

impl<'a> RunScope<'a> {
    /// The run's cancellation.
    pub(super) fn cancel(&self) -> &'a CancellationToken {
        self.target.trace.cancel()
    }

    /// Whether the run was cancelled.
    pub(super) fn is_cancelled(&self) -> bool {
        self.cancel().is_cancelled()
    }

    /// Record the custom entries extensions queued since the last step (pi: `appendEntry`),
    /// each announced as a `custom_entry` event. An error means the session could not record
    /// one.
    pub(super) async fn record_pending_entries(&mut self) -> anyhow::Result<()> {
        for entry in self.config.queues.take_entries() {
            let entry_id = self
                .transcript
                .append_custom_entry(&entry.custom_type, entry.data)
                .await
                .context(PERSIST_FAILED)?;
            self.target
                .emit(EventKind::CustomEntry {
                    entry_id,
                    custom_type: entry.custom_type,
                })
                .await;
        }
        Ok(())
    }

    /// Append `message`, which `actor` sent when it is a person's input, and which is the
    /// message at the front of a queue if `queued` names one, as the queue holds it: report
    /// its start, record it, then report its end. It leaves the queue as soon as it is
    /// recorded, before observers see it: a run dropped while they run cannot take the same
    /// input again. Returns its entry ID.
    pub(super) async fn append_from(
        &mut self,
        message: Message,
        actor: Option<String>,
        queued: Option<(Queue, Input)>,
    ) -> anyhow::Result<String> {
        self.target.emit(EventKind::MessageStart { role: message.role() }).await;
        // The event that reports the message owns it, so the session stores a copy.
        let entry_id = self
            .transcript
            .append_message_from(message.clone(), actor)
            .await
            .context(PERSIST_FAILED)?;
        if let Some((queue, original)) = queued {
            self.config.queues.remove_front(queue, &original);
        }
        self.target
            .emit(EventKind::MessageEnd {
                message,
                entry_id: entry_id.clone(),
                custom: None,
            })
            .await;
        Ok(entry_id)
    }

    /// The same scope, borrowed for a shorter time.
    pub(super) fn reborrow(&mut self) -> RunScope<'_> {
        RunScope {
            transcript: &mut *self.transcript,
            run_start: self.run_start,
            target: self.target,
            config: self.config,
            compaction: self.compaction,
            pause: &mut *self.pause,
            cost_budget: self.cost_budget,
        }
    }

    /// Open the run: announce it, record a result for each call an interrupted run left
    /// without one, and find the input the run starts with. An error says why the run cannot
    /// start.
    pub(super) async fn open(&mut self, begin: Start) -> Result<FirstInput, String> {
        let config = self.config;
        self.target
            .emit(EventKind::RunStart {
                model: config.model.clone(),
                provider: config.provider.name().into(),
            })
            .await;
        // A failed answer that a continuation drops goes before recovery would answer its
        // calls, which never ran.
        if matches!(begin, Start::Continue) {
            self.omit_failed_answer().await?;
        }
        // Before any system prompt change or input, so that results directly follow their calls.
        self.recover_interrupted_calls()
            .await
            .map_err(|error| format!("{error:#}"))?;
        match begin {
            Start::Message(message) => Ok(FirstInput::Message(message)),
            Start::Continue => self.continuation(),
        }
    }

    /// Compact with `trigger`, summarizing the old prefix; the pause follows how it ends. When
    /// it ends `Completed`, the session's projection is the new context. `attempt` is the
    /// attempt of the run that compacts, for the events of the summary; 1 outside a run. A
    /// summary that the run's model would make, without the prices its cost budget needs,
    /// fails before anything is recorded or requested.
    pub(super) async fn compact(&mut self, trigger: Trigger, attempt: u32) -> Compacted {
        // The kernel asks the model only when the host has no summarizer.
        if self.config.summarizer.is_none()
            && let Err(error) = check_priced(&self.config.models, &self.config.model, self.cost_budget)
        {
            return Compacted::without_request(0, End::Failed(error));
        }
        let compactor = Compactor {
            transcript: &mut *self.transcript,
            provider: self.config.provider.as_ref(),
            model: &self.config.model,
            models: &self.config.models,
            summarizer: self.config.summarizer.as_deref(),
            compaction: self.compaction,
            events: &self.target,
            // The field, not `cancel()`: the transcript above is borrowed mutably.
            cancel: self.target.trace.cancel(),
            trigger,
            attempt,
        };
        let compacted = compactor.run().await;
        self.pause.note(&compacted);
        compacted
    }

    /// Omit a failed or cancelled answer at the end of the conversation with a context edit,
    /// as a retry does: the model answers again.
    async fn omit_failed_answer(&mut self) -> Result<(), String> {
        match self.last_message() {
            LastMessage::Failed(failed) => self.omit_answer(&failed).await,
            LastMessage::Nothing | LastMessage::Unanswered | LastMessage::Answered => Ok(()),
        }
    }

    /// The input a continuation starts with, once [`omit_failed_answer`](Self::omit_failed_answer)
    /// has left out a failed answer at the end; an error when there is nothing for the model
    /// to answer. Queued input stays in its queue: the loop takes it as it does between steps,
    /// so that a message is taken only once it is recorded, and what a stopped run did not
    /// take is still queued.
    fn continuation(&self) -> Result<FirstInput, String> {
        match self.last_message() {
            LastMessage::Nothing => Err("nothing to continue: the conversation is empty; send a prompt instead".into()),
            LastMessage::Unanswered => Ok(FirstInput::Conversation),
            // The failed answer at the end was left out already: one here would be a second in
            // a row, which no run leaves.
            LastMessage::Failed(_) => {
                Err("nothing to continue: the conversation ends with a failed answer; send a prompt instead".into())
            }
            LastMessage::Answered => {
                let queues = &self.config.queues;
                if queues.has(Queue::Steering) {
                    // Every step takes steering first.
                    Ok(FirstInput::Conversation)
                } else if queues.has(Queue::FollowUp) {
                    Ok(FirstInput::QueuedFollowUps)
                } else {
                    Err(
                        "nothing to continue: the conversation ends with a finished answer; send a prompt instead"
                            .into(),
                    )
                }
            }
        }
    }

    /// How the conversation the model reads ends: its last message that is not a system
    /// message.
    fn last_message(&self) -> LastMessage {
        self.transcript
            .projection()
            .iter()
            .rev()
            .find_map(|item| match &item.message {
                Message::System(_) => None,
                Message::User { .. } | Message::ToolResult(_) => Some(LastMessage::Unanswered),
                Message::Assistant(answer) if matches!(answer.stop_reason, StopReason::Error | StopReason::Aborted) => {
                    Some(LastMessage::Failed(item.entry_id.clone()))
                }
                Message::Assistant(_) => Some(LastMessage::Answered),
            })
            .unwrap_or(LastMessage::Nothing)
    }

    /// Whether automatic compaction is on for the model the run asks now: the session's
    /// model, which a switch between runs changes.
    pub(super) fn compacts_automatically(&self) -> bool {
        self.compaction.automatic(&self.config.models, &self.config.model)
    }

    /// Whether the context passes a trigger of automatic compaction: its bytes, or the tokens
    /// it is estimated to hold, against the token trigger of the model the run asks now. Each
    /// is measured only when its trigger applies.
    fn over_trigger(&self) -> anyhow::Result<bool> {
        if let Some(limit) = self.compaction.trigger_bytes
            && compaction::context_bytes(&self.transcript.messages())? > limit
        {
            return Ok(true);
        }
        match self
            .compaction
            .trigger_tokens_for(&self.config.models, &self.config.model)
        {
            Some(limit) => Ok(compaction::context_tokens(self.transcript)? > limit),
            None => Ok(false),
        }
    }

    /// Prepare the context before a task request: compact first when automatic compaction
    /// is on and the context passes a trigger; what that records is in the projection
    /// afterwards. Returns how many model requests it took. They and their usage are counted
    /// into `accounting`, failed ones included, so that they count against the run's budget
    /// whatever the result.
    ///
    /// An error stops the loop: preparation failed and left the context unchanged
    /// ([`Halt::Failed`]), or the session could not record it ([`Halt::Unrecorded`]).
    pub(super) async fn prepare_context(&mut self, accounting: &mut RunAccounting) -> Result<u32, Halt> {
        if !self.compacts_automatically() {
            return Ok(0);
        }
        let over_trigger = self
            .over_trigger()
            .map_err(|error| Halt::Failed(format!("cannot measure context: {error:#}")))?;
        if !over_trigger {
            return Ok(0);
        }
        if let Some(paused) = self.pause.refusal() {
            return Err(Halt::Failed(format!(
                "{paused}; compact manually to retry, start a new session, or disable automatic compaction"
            )));
        }
        let compacted = self.compact_for_run(accounting).await;
        match compacted.end {
            End::Completed { .. } => Ok(compacted.reply.requests()),
            End::Unrecorded(error) => Err(Halt::Unrecorded(error)),
            End::Failed(error) | End::Cancelled(error) => Err(Halt::Failed(error)),
            End::Noop => Err(Halt::Failed(
                "context exceeds its compaction trigger but cannot be compacted while preserving \
                 system messages and the latest input; raise the trigger or start a new session"
                    .into(),
            )),
        }
    }

    /// Compact on the run's initiative, before a request or to recover from a context
    /// overflow, counting the summary's requests and usage into `accounting`.
    pub(super) async fn compact_for_run(&mut self, accounting: &mut RunAccounting) -> Compacted {
        let compacted = self.compact(Trigger::Automatic, accounting.attempts).await;
        accounting.count_requests(compacted.reply.requests(), &compacted.usage);
        compacted
    }
}
