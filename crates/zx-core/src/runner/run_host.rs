//! What a run lends the loop and the compactor: the session it records in, where its events
//! go, and the model it summarizes with. The Runner builds one per run, and each attempt's
//! loop records through it: recording goes to the session as it would without a run, and
//! preparing the context may summarize it (pi: AgentSession writes on `message_end`).

use super::{Ending, EventTarget, FirstInput, Halt, LoopConfig, RunAccounting, Start};
use crate::ai::{Message, StopReason};
use crate::compaction::{self, Compacted, Compactor, End, TokenTrigger, Trigger};
use crate::events::EventKind;
use crate::input::Queue;
use crate::transcript::Transcript;

/// How a run ends: as its last attempt ended, or failed for a reason.
pub(super) enum RunStop {
    /// The last attempt ended this way, which explains itself.
    Ended(Ending),
    /// The run failed, for this reason.
    Failed(String),
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

    /// Pause after a paid failure, resume after a success; other ends change nothing.
    fn note(&mut self, compacted: &Compacted) {
        let paid = compacted.requests > 0;
        match &compacted.end {
            End::Completed { .. } => self.0 = None,
            End::Failed(error) if paid => self.0 = Some(error.clone()),
            End::Unrecorded(error) if paid => self.0 = Some(format!("{error:#}")),
            End::Failed(_) | End::Unrecorded(_) | End::Noop | End::Cancelled(_) => {}
        }
    }
}

/// A run's borrow of the Runner: the session, the configuration with its hooks and model,
/// and where the run's events go.
pub(super) struct RunHost<'a> {
    pub(super) session: &'a mut Transcript,
    /// How many entries the session had when the run started: what follows is the run's.
    pub(super) run_start: usize,
    pub(super) target: EventTarget<'a>,
    pub(super) config: &'a LoopConfig,
    pub(super) compaction: &'a compaction::Config,
    pub(super) pause: &'a mut CompactionPause,
}

impl RunHost<'_> {
    /// The same host, borrowed for a shorter time.
    pub(super) fn reborrow(&mut self) -> RunHost<'_> {
        RunHost {
            session: &mut *self.session,
            run_start: self.run_start,
            target: self.target,
            config: self.config,
            compaction: self.compaction,
            pause: &mut *self.pause,
        }
    }

    /// Open the run: announce it, record a result for each call an interrupted run left
    /// without one, and find the input the run starts with. An error says why the run cannot
    /// start.
    pub(super) async fn open_run(&mut self, begin: Start) -> Result<FirstInput, String> {
        let config = self.config;
        self.target
            .emit(EventKind::RunStart {
                model: config.model.clone(),
                provider: config.provider.name().into(),
            })
            .await;
        // Before any system prompt change or input, so that results directly follow their calls.
        self.recover_interrupted_calls()
            .await
            .map_err(|error| format!("{error:#}"))?;
        match begin {
            Start::Message(message) => Ok(FirstInput::Prompt(message)),
            Start::Continue => self.continuation().await,
        }
    }

    /// Compact with `trigger`, summarizing the old prefix; the pause follows how it ends. When
    /// it ends `Completed`, the session's projection is the new context. `attempt` is the
    /// attempt of the run that compacts, for the events of the summary; 1 outside a run.
    pub(super) async fn compact(&mut self, trigger: Trigger, attempt: u32) -> Compacted {
        let compactor = Compactor {
            session: &mut *self.session,
            provider: self.config.provider.as_ref(),
            model: &self.config.model,
            models: &self.config.models,
            summarizer: self.config.summarizer.as_deref(),
            compaction: self.compaction,
            events: &self.target,
            cancel: self.target.trace.cancel(),
            trigger,
            attempt,
        };
        let compacted = compactor.run().await;
        self.pause.note(&compacted);
        compacted
    }

    /// The input a continuation starts with, after omitting a failed answer at the end; an
    /// error when there is nothing for the model to answer. Queued input stays in its queue:
    /// the loop takes it as it does between steps, so that a message is taken only once it
    /// is recorded, and what a stopped run did not take is still queued.
    async fn continuation(&mut self) -> Result<FirstInput, String> {
        let projection = self.session.projection();
        let Some(last) = projection
            .iter()
            .rev()
            .find(|item| !matches!(item.message, Message::System(_)))
        else {
            return Err("nothing to continue: the conversation is empty; send a prompt instead".into());
        };
        match &last.message {
            Message::User { .. } | Message::ToolResult(_) => Ok(FirstInput::Conversation),
            Message::Assistant(answer) if matches!(answer.stop_reason, StopReason::Error | StopReason::Aborted) => {
                let failed = last.entry_id.clone();
                self.omit_failed_attempt(&failed).await?;
                Ok(FirstInput::Conversation)
            }
            Message::Assistant(_) => {
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
            Message::System(_) => Err("nothing to continue: the conversation has only system messages".into()),
        }
    }

    /// Whether the context passes a trigger of automatic compaction: its bytes, or the tokens
    /// it is estimated to hold. Each is measured only when its trigger is set.
    fn over_trigger(&self) -> anyhow::Result<bool> {
        if let Some(limit) = self.compaction.trigger_bytes
            && compaction::context_bytes(&self.session.messages())? > limit
        {
            return Ok(true);
        }
        match self.compaction.trigger_tokens {
            TokenTrigger::Above(limit) => Ok(compaction::context_tokens(self.session)? > limit),
            TokenTrigger::Unset | TokenTrigger::Off => Ok(false),
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
        if !self.compaction.automatic() {
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
        let compacted = self.compact_for_run(accounting).await.map_err(Halt::Failed)?;
        match compacted.end {
            End::Completed { .. } => Ok(compacted.requests),
            End::Unrecorded(error) => Err(Halt::Unrecorded(error)),
            End::Failed(error) | End::Cancelled(error) => Err(Halt::Failed(error)),
            End::Noop => Err(Halt::Failed(
                "context exceeds its compaction trigger but cannot be compacted while preserving \
                 system messages and the latest input; raise the trigger or start a new session"
                    .into(),
            )),
        }
    }

    /// Automatic summaries, including overflow recovery, obey the same model price check
    /// and contribute to the same run accounting.
    pub(super) async fn compact_for_run(&mut self, accounting: &mut RunAccounting) -> Result<Compacted, String> {
        if self.config.summarizer.is_none() {
            accounting.check_model(&self.config.models, &self.config.model)?;
        }
        let compacted = self.compact(Trigger::Automatic, accounting.attempts).await;
        accounting.count_requests(compacted.requests, &compacted.usage);
        Ok(compacted)
    }
}
