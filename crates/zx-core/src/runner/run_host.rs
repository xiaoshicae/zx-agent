//! What a run lends the loop and the summarizer: the session it records in, the hooks and
//! sink it reports to, and the model it summarizes with. The Runner builds one per run and
//! the loop reaches it as its [`SessionHost`] (pi: AgentSession writes on `message_end`).

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::accounting::RunAccounting;
use super::final_answer::final_answer;
use super::{Limits, RunTimer, Start, conclude};
use crate::agent::{
    AgentEvent, ContextPreparation, EventTarget, LoopConfig, LoopOutcome, Prepared, QueuedMessage, RunReport,
    SessionHost, record_pending_entries,
};
use crate::ai::retry::RetryPolicy;
use crate::ai::{AssistantMessage, Message, Provider, Request, StopReason, ToolCall, duration_ms};
use crate::compaction::{self, Summarizer, Trigger};
use crate::session::{CustomMessage, Entry, RequestSource, SessionStore};

/// How a run ends when it stops before a finished answer.
pub(super) enum RunStop {
    /// The last attempt ended with this outcome, which explains itself; never
    /// [`LoopOutcome::Error`], which `Failed` states with its reason.
    Ended(LoopOutcome),
    /// The run failed, for this reason.
    Failed(String),
}

/// The text for an entry the session could not record, with the cause.
pub(super) fn persist_error(error: &anyhow::Error) -> String {
    format!("cannot persist session entry: {error:#}")
}

/// Why automatic summaries are paused: a paid summary request failed, and repeating it on
/// every prompt would keep paying for the same failure. A summary that succeeds, a manual
/// one included, or a change of model resumes them. The Runner owns it, so the pause
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
        let spec = &self.config.spec;
        self.emit(AgentEvent::RunStart {
            model: spec.model.clone(),
            provider: spec.provider.name().into(),
        })
        .await;
        // Entry IDs remain stable even when hooks omit messages from earlier runs.
        let run_start = self.session.entries().len();
        // Before any system prompt change or input, so that results directly follow their calls.
        let prompts = match (self.recover_interrupted_calls().await, begin) {
            (Err(error), _) => Err(error),
            (Ok(()), Start::Message(message)) => Ok(vec![message]),
            (Ok(()), Start::Continue) => self.continuation().await,
        };
        (prompts, run_start)
    }

    /// Close the run: keep the state hooks asked to record, then report how the run ended
    /// and the answer it added after the first `run_start` entries.
    pub(super) async fn close_run(
        &mut self,
        stop: RunStop,
        run_start: usize,
        limits: &Limits,
        accounting: &RunAccounting,
        timer: &RunTimer,
    ) -> RunReport {
        // State that hooks asked to record during the last events is kept before the run is
        // reported, so a host reading the session afterwards finds it.
        let config = self.config;
        let target = self.target;
        let stop = match record_pending_entries(config, self, target).await {
            Ok(()) => stop,
            Err(error) => RunStop::Failed(error),
        };
        let (outcome, error) = conclude(stop, limits, accounting, timer.timed_out());
        let report = RunReport {
            outcome,
            final_text: final_answer(&self.session.messages_since(run_start)),
            model_requests: accounting.model_requests,
            retries: accounting.retries,
            usage: accounting.usage.clone(),
            error,
            session_file: self.session.path().map(|path| path.display().to_string()),
        };
        self.emit(AgentEvent::RunEnd {
            report: Box::new(report.clone()),
        })
        .await;
        report
    }

    /// Summarize the old prefix with `trigger`; the pause follows the outcome. When the
    /// report says `Completed`, the session's projection is the new context.
    pub(super) async fn summarize(&mut self, trigger: Trigger, cancel: &CancellationToken) -> compaction::Report {
        let mut summarizer = Summarizer {
            session: &mut *self.session,
            provider: self.config.spec.provider.as_ref(),
            model: &self.config.spec.model,
            config: self.compaction,
            target: self.target,
            hooks: &self.config.hooks,
            attempt: self.attempt,
            models: &self.config.models,
        };
        let report = summarizer.run(trigger, cancel).await;
        self.pause.note(&report);
        report
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
                entry_id: Some(edit),
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
        failed_entry: Option<&str>,
        accounting: &mut RunAccounting,
        cancel: &CancellationToken,
    ) -> Result<(), RunStop> {
        self.emit(AgentEvent::ContextOverflow { error: error.into() }).await;
        if let Some(target) = failed_entry {
            self.omit_failed_attempt(target).await.map_err(RunStop::Failed)?;
        }
        let report = self.summarize(Trigger::Overflow, cancel).await;
        accounting.model_requests = accounting.model_requests.saturating_add(report.requests);
        accounting.usage.add(&report.usage);
        if let Some(error) = report.persistence_error {
            return Err(RunStop::Failed(error));
        }
        if report.outcome == compaction::Outcome::Completed {
            return Ok(());
        }
        if report.outcome == compaction::Outcome::Aborted {
            return Err(RunStop::Ended(LoopOutcome::Aborted));
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
        failed_entry: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<(), RunStop> {
        accounting.retries += 1;
        let delay = retry.delay_for(accounting.retries, failed);
        self.emit(AgentEvent::RetryScheduled {
            attempt: accounting.retries,
            max_retries: retry.max_retries,
            delay_ms: duration_ms(delay),
            error: failed.failure_reason().to_string(),
        })
        .await;
        if let Some(target) = failed_entry {
            self.omit_failed_attempt(target).await.map_err(RunStop::Failed)?;
        }
        tokio::select! {
            () = tokio::time::sleep(delay) => Ok(()),
            () = cancel.cancelled() => Err(RunStop::Ended(LoopOutcome::Aborted)),
        }
    }
}

#[async_trait]
impl SessionHost for RunHost<'_> {
    async fn append_message(&mut self, message: &Message) -> Result<Option<String>, String> {
        // The event that reports the message owns it, so the session stores a copy.
        self.session
            .append_message(message.clone())
            .await
            .map(Some)
            .map_err(|error| persist_error(&error))
    }

    async fn append_custom_message(&mut self, message: &CustomMessage) -> Result<(Option<String>, Message), String> {
        let id = self
            .session
            .append_custom_message(message.clone())
            .await
            .map_err(|error| persist_error(&error))?;
        match self.session.get(&id) {
            Some(Entry::CustomMessage(entry)) => Ok((Some(id), entry.projected())),
            _ => Err(format!("custom message {id} was not recorded")),
        }
    }

    async fn append_custom_entry(&mut self, custom_type: &str, data: Value) -> Result<Option<String>, String> {
        self.session
            .append_custom_entry(custom_type, data)
            .await
            .map(Some)
            .map_err(|error| persist_error(&error))
    }

    async fn edit_context(&mut self, index: usize, replacement: Option<&str>) -> Result<Option<String>, String> {
        let projection = self.session.projection();
        let target = projection
            .get(index)
            .ok_or_else(|| format!("context index {index} is out of bounds"))?;
        self.session
            .append_context_edit(&target.entry_id, replacement.map(str::to_string))
            .await
            .map(Some)
            .map_err(|error| format!("cannot persist context edit: {error:#}"))
    }

    async fn record_request(&mut self, request: &Request, provider: &dyn Provider) -> Result<Option<String>, String> {
        let options = provider.request_options();
        self.session
            .record_request(request, provider.name(), options, RequestSource::Projection)
            .await
            .map(Some)
            .map_err(|error| format!("cannot persist request checkpoint: {error:#}"))
    }

    async fn record_tool_call_started(&mut self, call: &ToolCall) -> Result<(), String> {
        self.session
            .append_tool_call_started(&call.id, &call.name)
            .await
            .map(|_| ())
            .map_err(|error| format!("cannot persist the start of tool call {:?}: {error:#}", call.id))
    }

    async fn prepare_context(&mut self, cancel: &CancellationToken) -> ContextPreparation {
        if !self.compaction.automatic() {
            return ContextPreparation::unchanged();
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
            Ok(false) => return ContextPreparation::unchanged(),
            Ok(true) => {}
            Err(error) => return ContextPreparation::failed(format!("cannot measure context: {error:#}")),
        }
        if let Some(reason) = self.pause.reason() {
            return ContextPreparation::failed(format!(
                "automatic compaction is paused because it failed earlier in this session ({reason}); \
                 compact manually to retry, start a new session, or disable automatic compaction"
            ));
        }
        let report = self.summarize(Trigger::Automatic, cancel).await;
        let result = if let Some(error) = report.persistence_error {
            Prepared::Unrecorded(error)
        } else if report.outcome == compaction::Outcome::Completed {
            Prepared::Updated(self.session.messages())
        } else {
            Prepared::Failed(
                report
                    .error
                    .unwrap_or_else(|| "automatic compaction made no progress".into()),
            )
        };
        ContextPreparation {
            requests: report.requests,
            usage: report.usage,
            result,
        }
    }
}
