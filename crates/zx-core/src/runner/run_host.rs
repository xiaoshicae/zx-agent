//! What a run lends the loop and the compactor: the session it records in, the hooks and
//! sink it reports to, and the model it summarizes with. The Runner builds one per run and
//! the loop reaches it as its [`SessionHost`]: recording goes to the session as it would
//! without a run, and preparing the context may summarize it (pi: AgentSession writes on
//! `message_end`).

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::Start;
use crate::agent::{
    AgentEvent, ContextPreparation, EventTarget, Halt, LoopConfig, QueuedMessage, RunAccounting, RunOutcome,
    SessionHost,
};
use crate::ai::retry::RetryPolicy;
use crate::ai::{AssistantMessage, Message, StopReason, duration_ms};
use crate::compaction::{self, Compacted, Compactor, Trigger};
use crate::session::SessionStore;

/// How a run ends when it stops before a finished answer.
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

    /// Pause after a paid failure, resume after a success; other outcomes change nothing.
    fn note(&mut self, report: &compaction::Report) {
        match report.outcome {
            compaction::Outcome::Completed => self.0 = None,
            compaction::Outcome::Failed if report.requests > 0 => {
                self.0 = Some(
                    report
                        .error
                        .clone()
                        .unwrap_or_else(|| "compaction made no progress".into()),
                );
            }
            compaction::Outcome::Failed | compaction::Outcome::Noop | compaction::Outcome::Aborted => {}
        }
    }
}

/// A run's borrow of the Runner: the session, the configuration with its hooks and model,
/// and the event sink.
pub(super) struct RunHost<'a> {
    pub session: &'a mut SessionStore,
    pub target: EventTarget<'a>,
    pub config: &'a LoopConfig,
    pub compaction: &'a compaction::Config,
    pub pause: &'a mut CompactionPause,
    /// The attempt the host serves, for the events of a summary.
    pub attempt: u32,
}

impl<'a> RunHost<'a> {
    pub(super) fn new(
        session: &'a mut SessionStore,
        target: EventTarget<'a>,
        config: &'a LoopConfig,
        compaction: &'a compaction::Config,
        pause: &'a mut CompactionPause,
    ) -> Self {
        Self {
            session,
            target,
            config,
            compaction,
            pause,
            attempt: 1,
        }
    }

    pub(super) async fn emit(&self, event: AgentEvent) {
        self.config.hooks.emit(event, self.target).await;
    }

    /// Open the run: announce it, record a result for each call an interrupted run left
    /// without one, and find the input the run starts with. Returns that input, or why the
    /// run cannot start, and the number of entries the session had: the run's answer is
    /// what it adds after them.
    pub(super) async fn open_run(&mut self, begin: Start) -> (Result<Vec<QueuedMessage>, String>, usize) {
        let config = self.config;
        self.emit(AgentEvent::RunStart {
            model: config.model.clone(),
            provider: config.provider.name().into(),
        })
        .await;
        // Entry IDs remain stable even when hooks omit messages from earlier runs.
        let run_start = self.session.entries().len();
        // Before any system prompt change or input, so that results directly follow their calls.
        let prompts = match (self.recover_interrupted_calls().await, begin) {
            (Err(error), _) => Err(format!("{error:#}")),
            (Ok(()), Start::Message(message)) => Ok(vec![message]),
            (Ok(()), Start::Continue) => self.continuation().await,
        };
        (prompts, run_start)
    }

    /// Compact with `trigger`, summarizing the old prefix; the pause follows the outcome. When the
    /// report says `Completed`, the session's projection is the new context.
    pub(super) async fn compact(&mut self, trigger: Trigger, cancel: &CancellationToken) -> Compacted {
        let mut compactor = Compactor {
            session: &mut *self.session,
            config: self.config,
            compaction: self.compaction,
            target: self.target,
            attempt: self.attempt,
        };
        let compacted = compactor.run(trigger, cancel).await;
        self.pause.note(&compacted.report);
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

    /// Keep a failed answer in the session but drop it from the model context, so that the
    /// next attempt continues from the message before it without repeating the user message.
    pub(super) async fn omit_failed_attempt(&mut self, target: &str) -> Result<(), String> {
        let index = self
            .session
            .projection()
            .iter()
            .position(|item| item.entry_id == target);
        let edit = self
            .session
            .append_context_edit(target, None)
            .await
            .map_err(|error| format!("cannot omit the failed attempt: {error:#}"))?;
        if let Some(index) = index {
            self.emit(AgentEvent::ContextEdited {
                index,
                replacement: None,
                entry_id: edit,
            })
            .await;
        }
        Ok(())
    }

    /// Omit the failed answer and summarize the old prefix, so that a new attempt can continue
    /// from the compacted context. The summary's cost is
    /// added to `accounting` whether or not it succeeds.
    pub(super) async fn recover_overflow(
        &mut self,
        error: &str,
        failed_entry: &str,
        accounting: &mut RunAccounting,
        cancel: &CancellationToken,
    ) -> Result<(), RunStop> {
        self.emit(AgentEvent::ContextOverflow { error: error.into() }).await;
        self.omit_failed_attempt(failed_entry).await.map_err(RunStop::Failed)?;
        let Compacted { report, unrecorded } = self.compact(Trigger::Overflow, cancel).await;
        accounting.count_requests(report.requests, &report.usage);
        if let Some(error) = unrecorded {
            return Err(RunStop::Failed(format!("{error:#}")));
        }
        if report.outcome == compaction::Outcome::Completed {
            return Ok(());
        }
        if report.outcome == compaction::Outcome::Aborted {
            return Err(RunStop::Ended(RunOutcome::Aborted));
        }
        let reason = report.error.unwrap_or_else(|| "compaction made no progress".into());
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
        cancel: &CancellationToken,
    ) -> Result<(), RunStop> {
        accounting.retries += 1;
        let delay = retry.delay_for(accounting.retries, failed);
        self.emit(AgentEvent::RetryScheduled {
            retry: accounting.retries,
            max_retries: retry.max_retries,
            delay_ms: duration_ms(delay),
            error: failed.failure_reason().to_string(),
        })
        .await;
        self.omit_failed_attempt(failed_entry).await.map_err(RunStop::Failed)?;
        tokio::select! {
            () = tokio::time::sleep(delay) => Ok(()),
            () = cancel.cancelled() => Err(RunStop::Ended(RunOutcome::Aborted)),
        }
    }
}

#[async_trait]
impl SessionHost for RunHost<'_> {
    fn session(&mut self) -> &mut SessionStore {
        self.session
    }

    /// Compact first when automatic compaction is on and the context passes a trigger.
    async fn prepare_context(&mut self, cancel: &CancellationToken) -> ContextPreparation {
        if !self.compaction.automatic() {
            return ContextPreparation::ready();
        }
        // Either trigger runs a summary: the bytes of the context, or the tokens the last
        // response reported plus an estimate for what came after it.
        let messages = self.session.messages();
        let measured = compaction::context_bytes(&messages).and_then(|bytes| {
            let over_bytes = self.compaction.trigger_bytes.is_some_and(|limit| bytes > limit);
            let over_tokens = match self.compaction.trigger_tokens {
                Some(limit) => compaction::context_tokens(&messages)? > limit,
                None => false,
            };
            Ok(over_bytes || over_tokens)
        });
        match measured {
            Ok(false) => return ContextPreparation::ready(),
            Ok(true) => {}
            Err(error) => return ContextPreparation::failed(format!("cannot measure context: {error:#}")),
        }
        if let Some(reason) = self.pause.reason() {
            return ContextPreparation::failed(format!(
                "automatic compaction is paused because it failed earlier in this session ({reason}); \
                 compact manually to retry, start a new session, or disable automatic compaction"
            ));
        }
        let Compacted { report, unrecorded } = self.compact(Trigger::Automatic, cancel).await;
        let result = match unrecorded {
            Some(error) => Err(Halt::Unrecorded(error)),
            None if report.outcome == compaction::Outcome::Completed => Ok(()),
            None => Err(Halt::Failed(
                report
                    .error
                    .unwrap_or_else(|| "automatic compaction made no progress".into()),
            )),
        };
        ContextPreparation {
            requests: report.requests,
            usage: report.usage,
            result,
        }
    }
}
