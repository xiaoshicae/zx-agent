//! What a run lends the loop and the compactor: the session it records in, the hooks and
//! sink it reports to, and the model it summarizes with. The Runner builds one per run and
//! the loop reaches it as its [`SessionHost`]: recording goes to the session as it would
//! without a run, and preparing the context may summarize it (pi: AgentSession writes on
//! `message_end`).

use async_trait::async_trait;

use super::Start;
use crate::agent::{AgentEvent, EventTarget, Halt, LoopConfig, QueuedMessage, RunAccounting, RunOutcome, SessionHost};
use crate::ai::retry::RetryPolicy;
use crate::ai::{AssistantMessage, Message, StopReason, duration_ms};
use crate::compaction::{self, Compacted, Compactor, End, Trigger};
use crate::session::SessionStore;

/// How a run ends: with the outcome of its last attempt, or failed for a reason.
pub(super) enum RunStop {
    /// The last attempt ended with this outcome, which explains itself; never
    /// [`RunOutcome::Error`], which `Failed` states with its reason.
    Ended(RunOutcome),
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
    pub(super) fn reason(&self) -> Option<&str> {
        self.0.as_deref()
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
            End::Failed(_) | End::Unrecorded(_) | End::Noop | End::Aborted(_) => {}
        }
    }
}

/// A run's borrow of the Runner: the session, the configuration with its hooks and model,
/// and the event sink.
pub(super) struct RunHost<'a> {
    pub(super) session: &'a mut SessionStore,
    pub(super) target: EventTarget<'a>,
    pub(super) config: &'a LoopConfig,
    pub(super) compaction: &'a compaction::Config,
    pub(super) pause: &'a mut CompactionPause,
}

impl RunHost<'_> {
    /// Open the run: announce it, record a result for each call an interrupted run left
    /// without one, and find the input the run starts with. An error says why the run cannot
    /// start.
    pub(super) async fn open_run(&mut self, begin: Start) -> Result<Vec<QueuedMessage>, String> {
        let config = self.config;
        self.target
            .emit(AgentEvent::RunStart {
                model: config.model.clone(),
                provider: config.provider.name().into(),
            })
            .await;
        // Before any system prompt change or input, so that results directly follow their calls.
        self.recover_interrupted_calls()
            .await
            .map_err(|error| format!("{error:#}"))?;
        match begin {
            Start::Message(message) => Ok(vec![message]),
            Start::Continue => self.continuation().await,
        }
    }

    /// Compact with `trigger`, summarizing the old prefix; the pause follows how it ends. When
    /// it ends `Completed`, the session's projection is the new context. `attempt` is the
    /// attempt of the run that compacts, for the events of the summary; 1 outside a run.
    pub(super) async fn compact(&mut self, trigger: Trigger, attempt: u32) -> Compacted {
        let compactor = Compactor {
            session: &mut *self.session,
            config: self.config,
            compaction: self.compaction,
            target: self.target,
            trigger,
            attempt,
        };
        let compacted = compactor.run().await;
        self.pause.note(&compacted);
        compacted
    }

    /// The input a continuation starts with, after omitting a failed answer at the end; an
    /// error when there is nothing for the model to answer.
    async fn continuation(&mut self) -> Result<Vec<QueuedMessage>, String> {
        let projection = self.session.projection();
        let Some(last) = projection
            .iter()
            .rev()
            .find(|item| !matches!(item.message, Message::System(_)))
        else {
            return Err("nothing to continue: the conversation is empty; send a prompt instead".into());
        };
        match &last.message {
            Message::User { .. } | Message::ToolResult(_) => Ok(Vec::new()),
            Message::Assistant(answer) if matches!(answer.stop_reason, StopReason::Error | StopReason::Aborted) => {
                let failed = last.entry_id.clone();
                self.omit_failed_attempt(&failed).await?;
                Ok(Vec::new())
            }
            Message::Assistant(_) => {
                let queues = &self.config.queues;
                let mut queued = queues.drain_steering();
                if queued.is_empty() {
                    queued = queues.drain_follow_up();
                }
                if queued.is_empty() {
                    return Err(
                        "nothing to continue: the conversation ends with a finished answer; send a prompt instead"
                            .into(),
                    );
                }
                Ok(queued)
            }
            Message::System(_) => Err("nothing to continue: the conversation has only system messages".into()),
        }
    }

    /// Keep the failed answer `entry_id` in the session but drop it from the model context, so
    /// that the next attempt continues from the message before it without repeating the user
    /// message.
    pub(super) async fn omit_failed_attempt(&mut self, entry_id: &str) -> Result<(), String> {
        let index = self
            .session
            .projection()
            .iter()
            .position(|item| item.entry_id == entry_id);
        let edit = self
            .session
            .append_context_edit(entry_id, None)
            .await
            .map_err(|error| format!("cannot omit the failed attempt: {error:#}"))?;
        if let Some(index) = index {
            self.target
                .emit(AgentEvent::ContextEdited {
                    index,
                    replacement: None,
                    entry_id: edit,
                })
                .await;
        }
        Ok(())
    }

    /// Omit the failed answer and summarize the old prefix, so that a new attempt can continue
    /// from the compacted context. The summary's cost is added to `accounting` whether or not
    /// it succeeds.
    pub(super) async fn recover_overflow(
        &mut self,
        error: &str,
        failed_entry: &str,
        accounting: &mut RunAccounting,
    ) -> Result<(), RunStop> {
        self.target
            .emit(AgentEvent::ContextOverflow { error: error.into() })
            .await;
        self.omit_failed_attempt(failed_entry).await.map_err(RunStop::Failed)?;
        let compacted = self.compact(Trigger::Automatic, accounting.attempts).await;
        accounting.count_requests(compacted.requests, &compacted.usage);
        let reason = match compacted.end {
            End::Completed { .. } => return Ok(()),
            End::Aborted(_) => return Err(RunStop::Ended(RunOutcome::Aborted)),
            End::Unrecorded(error) => return Err(RunStop::Failed(format!("{error:#}"))),
            End::Failed(reason) => reason,
            End::Noop => "the model rejected the context as too long but cannot compact while preserving \
                          system messages and the latest input; lower keepRecentBytes, use a model with a \
                          larger context or start a new session"
                .into(),
        };
        Err(RunStop::Failed(format!(
            "{error}; compacting the context to recover failed: {reason}"
        )))
    }

    /// Announce a retry, omit the failed answer from the context and wait out the backoff.
    /// Ends the run when the session cannot record the edit or the wait is cancelled.
    pub(super) async fn schedule_retry(
        &mut self,
        retry: &RetryPolicy,
        accounting: &mut RunAccounting,
        failed: &AssistantMessage,
        failed_entry: &str,
    ) -> Result<(), RunStop> {
        accounting.retries += 1;
        let delay = retry.delay_for(accounting.retries, failed);
        self.target
            .emit(AgentEvent::RetryScheduled {
                retry: accounting.retries,
                max_retries: retry.max_retries,
                delay_ms: duration_ms(delay),
                error: failed.failure_reason().to_string(),
            })
            .await;
        self.omit_failed_attempt(failed_entry).await.map_err(RunStop::Failed)?;
        tokio::select! {
            () = tokio::time::sleep(delay) => Ok(()),
            () = self.target.trace.cancel().cancelled() => Err(RunStop::Ended(RunOutcome::Aborted)),
        }
    }
}

#[async_trait]
impl SessionHost for RunHost<'_> {
    fn session(&mut self) -> &mut SessionStore {
        self.session
    }

    /// Compact first when automatic compaction is on and the context passes a trigger.
    async fn prepare_context(&mut self, accounting: &mut RunAccounting) -> Result<u32, Halt> {
        if !self.compaction.automatic() {
            return Ok(0);
        }
        // Either trigger runs a summary: the bytes of the context, or the tokens the last
        // response reported plus an estimate for what came after it.
        let messages = self.session.messages();
        let over_trigger = compaction::context_bytes(&messages)
            .and_then(|bytes| {
                let over_bytes = self.compaction.trigger_bytes.is_some_and(|limit| bytes > limit);
                let over_tokens = match self.compaction.trigger_tokens {
                    Some(limit) => compaction::context_tokens(&messages)? > limit,
                    None => false,
                };
                Ok(over_bytes || over_tokens)
            })
            .map_err(|error| Halt::Failed(format!("cannot measure context: {error:#}")))?;
        if !over_trigger {
            return Ok(0);
        }
        if let Some(reason) = self.pause.reason() {
            return Err(Halt::Failed(format!(
                "automatic compaction is paused because it failed earlier in this session ({reason}); \
                 compact manually to retry, start a new session, or disable automatic compaction"
            )));
        }
        let compacted = self.compact(Trigger::Automatic, accounting.attempts).await;
        accounting.count_requests(compacted.requests, &compacted.usage);
        match compacted.end {
            End::Completed { .. } => Ok(compacted.requests),
            End::Unrecorded(error) => Err(Halt::Unrecorded(error)),
            End::Failed(error) | End::Aborted(error) => Err(Halt::Failed(error)),
            End::Noop => Err(Halt::Failed(
                "context exceeds its compaction trigger but cannot be compacted while preserving \
                 system messages and the latest input; raise the trigger or start a new session"
                    .into(),
            )),
        }
    }
}
