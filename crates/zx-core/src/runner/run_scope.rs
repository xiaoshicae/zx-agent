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
use crate::compaction::{self, Compacted, Compactor, End, Initiative, SummaryReply};
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

/// The sizes of the context above which automatic compaction runs: its bytes, and the tokens
/// it is estimated to hold; `None` where that measure starts none.
#[derive(Clone, Copy, Debug)]
struct Triggers {
    bytes: Option<usize>,
    tokens: Option<u64>,
}

/// Why automatic compaction is paused: a paid summary request failed, and repeating it on
/// every prompt would keep paying for the same failure. It tries once more after the context
/// has grown by what separates two compactions that succeed, so that a failure that repeats
/// costs no more often than compacting does. A compaction that succeeds, a manual one
/// included, or a change of model resumes it. The Runner owns it, so the pause outlives one
/// prompt and a run whose future is dropped.
#[derive(Default)]
pub(super) struct CompactionPause(Option<Paused>);

/// A pause of automatic compaction.
struct Paused {
    /// Why the summary failed.
    reason: String,
    /// The triggers automatic compaction waits for instead of its own while paused.
    resume_above: Triggers,
}

impl CompactionPause {
    /// The triggers automatic compaction waits for while it is paused.
    fn resume_above(&self) -> Option<Triggers> {
        self.0.as_ref().map(|paused| paused.resume_above)
    }

    /// Why automatic compaction does not run now, when it is paused.
    pub(super) fn refusal(&self) -> Option<String> {
        let reason = &self.0.as_ref()?.reason;
        Some(format!(
            "automatic compaction is paused because it failed earlier in this session ({reason}); \
             it is tried again once the context has grown"
        ))
    }

    pub(super) fn clear(&mut self) {
        self.0 = None;
    }

    /// Pause after a failure the summary request was answered for, which asking again would
    /// pay for again, until the context passes `resume_above()`; resume after a success.
    /// Other ends change nothing: a request that got no answer may succeed the next time.
    fn note(&mut self, compacted: &Compacted, resume_above: impl FnOnce() -> Triggers) {
        let answered = compacted.reply == SummaryReply::Answered;
        let reason = match &compacted.end {
            End::Completed { .. } => return self.clear(),
            End::Failed(error) if answered => error.clone(),
            End::Unrecorded(error) if answered => format!("{error:#}"),
            End::Failed(_) | End::Unrecorded(_) | End::Noop | End::Cancelled(_) => return,
        };
        self.0 = Some(Paused {
            reason,
            resume_above: resume_above(),
        });
    }
}

/// The triggers a paused automatic compaction waits for, from the context of `transcript` as
/// it is now: each of `triggers`, raised to the context's size now plus the growth between two
/// compactions that succeed, which keep about `keep_recent_bytes` of the context.
fn resume_above(transcript: &Transcript, compaction: &compaction::Config, triggers: Triggers) -> Triggers {
    let kept_bytes = compaction.keep_recent_bytes;
    // A measure that fails starts no compaction: the context it cannot measure stops the next
    // run that compacts automatically before its trigger is checked (`prepare_context`).
    Triggers {
        bytes: triggers.bytes.and_then(|trigger| {
            let now = compaction::context_bytes(&transcript.messages()).ok()?;
            Some(now.saturating_add(trigger.saturating_sub(kept_bytes)))
        }),
        tokens: triggers.tokens.and_then(|trigger| {
            let now = compaction::context_tokens(transcript).ok()?;
            Some(now.saturating_add(trigger.saturating_sub(compaction::estimated_tokens(kept_bytes))))
        }),
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
    pub(super) async fn compact(&mut self, initiative: Initiative, attempt: u32) -> Compacted {
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
            initiative,
            attempt,
        };
        let compacted = compactor.run().await;
        let triggers = self.triggers();
        let (transcript, compaction) = (&*self.transcript, self.compaction);
        self.pause
            .note(&compacted, || resume_above(transcript, compaction, triggers));
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

    /// The triggers of automatic compaction: the byte trigger, and the token trigger of the
    /// model the run asks now.
    fn triggers(&self) -> Triggers {
        Triggers {
            bytes: self.compaction.trigger_bytes,
            tokens: self
                .compaction
                .trigger_tokens_for(&self.config.models, &self.config.model),
        }
    }

    /// Whether the context passes one of `triggers`: its bytes, or the tokens it is estimated
    /// to hold. Each is measured only when its trigger applies.
    fn passes(&self, triggers: Triggers) -> anyhow::Result<bool> {
        if let Some(limit) = triggers.bytes
            && compaction::context_bytes(&self.transcript.messages())? > limit
        {
            return Ok(true);
        }
        match triggers.tokens {
            Some(limit) => Ok(compaction::context_tokens(self.transcript)? > limit),
            None => Ok(false),
        }
    }

    /// Prepare the context before a task request: compact first when automatic compaction
    /// is on and the context passes a trigger; what that records is in the projection
    /// afterwards. Returns how many model requests changed the context: none when there was
    /// nothing to do or the summary failed. Every summary request and its usage are counted
    /// into `accounting`, failed ones included, so that they count against the run's budget
    /// whatever the result.
    ///
    /// A trigger lies below what the model accepts, so a compaction that cannot bring the
    /// context under it, because the summary failed or because there is nothing old enough to
    /// summarize, leaves the context as it was and the run goes on with it: only the model
    /// rejecting the context as too long makes a summary necessary, which recovering from
    /// that overflow tries. The run does not try again before its later requests. While
    /// automatic compaction is paused, runs wait for the higher triggers of the pause instead.
    ///
    /// An error stops the loop: the context cannot be measured or the compaction was
    /// cancelled ([`Halt::Failed`]), or the session could not record it
    /// ([`Halt::Unrecorded`]).
    pub(super) async fn prepare_context(&mut self, accounting: &mut RunAccounting) -> Result<u32, Halt> {
        if !self.compacts_automatically() {
            return Ok(0);
        }
        let triggers = self.pause.resume_above().unwrap_or_else(|| self.triggers());
        let over_trigger = self
            .passes(triggers)
            .map_err(|error| Halt::Failed(format!("cannot measure context: {error:#}")))?;
        if !over_trigger || accounting.failed_compactions > 0 {
            return Ok(0);
        }
        let compacted = self.compact_for_run(accounting).await;
        match compacted.end {
            End::Completed { .. } => Ok(compacted.reply.requests()),
            End::Failed(_) | End::Noop => {
                accounting.failed_compactions = accounting.failed_compactions.saturating_add(1);
                Ok(0)
            }
            End::Unrecorded(error) => Err(Halt::Unrecorded(error)),
            End::Cancelled(error) => Err(Halt::Failed(error)),
        }
    }

    /// Compact on the run's initiative, before a request or to recover from a context
    /// overflow, counting the summary's requests and usage into `accounting`.
    pub(super) async fn compact_for_run(&mut self, accounting: &mut RunAccounting) -> Compacted {
        let compacted = self.compact(Initiative::Automatic, accounting.attempts).await;
        accounting.count_requests(compacted.reply.requests(), &compacted.usage);
        compacted
    }
}
