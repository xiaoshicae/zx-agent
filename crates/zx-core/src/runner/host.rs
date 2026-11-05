//! What a run lends the loop and the summary host: the session it records in, the hooks and
//! sink it reports to, and the model it summarizes with. The Runner builds one per run and
//! the loop reaches it as its [`SessionHost`] (pi: AgentSession writes on `message_end`).

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::accounting::RunAccounting;
use crate::agent::{
    AgentEvent, ContextPreparation, Emit, HookChain, LoopConfig, LoopOutcome, MessageQueues, Prepared, SessionHost,
};
use crate::ai::retry::RetryPolicy;
use crate::ai::{AssistantMessage, Message, Provider, Request, StopReason, ToolCall, duration_ms};
use crate::compaction::{self, Trigger};
use crate::session::{RequestSource, SessionStore};

/// How a run ends when it stops before a finished answer.
pub(super) struct RunStop {
    pub outcome: LoopOutcome,
    /// The reason, for [`LoopOutcome::Error`]; the other outcomes explain themselves.
    pub error: Option<String>,
}

impl RunStop {
    pub fn error(reason: impl Into<String>) -> Self {
        Self {
            outcome: LoopOutcome::Error,
            error: Some(reason.into()),
        }
    }

    pub fn aborted() -> Self {
        Self {
            outcome: LoopOutcome::Aborted,
            error: None,
        }
    }
}

/// Why automatic summaries are paused: a paid summary request failed, and repeating it on
/// every prompt would keep paying for the same failure. A summary that succeeds, a manual
/// one included, or a change of model resumes them. The Runner owns it, so the pause
/// outlives one prompt and a run whose future is dropped.
#[derive(Default)]
pub(super) struct CompactionPause(Option<String>);

impl CompactionPause {
    pub fn reason(&self) -> Option<&str> {
        self.0.as_deref()
    }

    pub fn clear(&mut self) {
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

/// A run's borrow of the Runner: the session, the hooks, the event sink and the model.
pub(super) struct RunHost<'a> {
    pub session: &'a mut SessionStore,
    pub to: Emit<'a>,
    pub hooks: &'a HookChain,
    pub provider: &'a dyn Provider,
    pub model: &'a str,
    pub compaction: &'a compaction::Config,
    pub pause: &'a mut CompactionPause,
    /// The attempt the host serves, for the events of a summary.
    pub attempt: u32,
}

impl<'a> RunHost<'a> {
    pub fn new(
        session: &'a mut SessionStore,
        to: Emit<'a>,
        config: &'a LoopConfig,
        compaction: &'a compaction::Config,
        pause: &'a mut CompactionPause,
    ) -> Self {
        Self {
            session,
            to,
            hooks: &config.hooks,
            provider: config.spec.provider.as_ref(),
            model: &config.spec.model,
            compaction,
            pause,
            attempt: 1,
        }
    }

    /// Summarize the old prefix with `trigger`; the pause follows the outcome. Returns the
    /// report and, when the summary succeeded, the new context.
    pub async fn summarize(
        &mut self,
        trigger: Trigger,
        cancel: &CancellationToken,
    ) -> (compaction::Report, Option<Vec<Message>>) {
        let mut host = compaction::Host {
            session: &mut *self.session,
            provider: self.provider,
            model: self.model,
            config: self.compaction,
            to: self.to,
            hooks: self.hooks,
            attempt: self.attempt,
        };
        let (report, messages) = host.run(trigger, cancel).await;
        self.pause.note(&report);
        (report, messages)
    }

    /// The input a continuation starts with, after omitting a failed answer at the end; an
    /// error when there is nothing for the model to answer.
    pub async fn continuation(&mut self, queues: &MessageQueues) -> Result<Vec<Message>, String> {
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
    /// next attempt continues from the message before it without repeating the user message
    /// (pi: AgentSession._prepareRetry and _omitRecoveryAttempt).
    pub async fn omit_failed_attempt(&mut self, target: &str) -> Result<(), String> {
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
            let event = AgentEvent::ContextEdited {
                index,
                replacement: None,
                entry_id: Some(edit),
            };
            self.hooks.emit(event, self.to).await;
        }
        Ok(())
    }

    /// Omit the failed answer and summarize the old prefix, so that a new attempt can continue
    /// from the compacted context (pi: AgentSession's overflow recovery). The summary's cost is
    /// added to `accounting` whether or not it succeeds.
    pub async fn recover_overflow(
        &mut self,
        error: &str,
        failed_entry: Option<&str>,
        accounting: &mut RunAccounting,
        cancel: &CancellationToken,
    ) -> Result<(), RunStop> {
        let overflow = AgentEvent::ContextOverflow { error: error.into() };
        self.hooks.emit(overflow, self.to).await;
        if let Some(target) = failed_entry {
            self.omit_failed_attempt(target).await.map_err(RunStop::error)?;
        }
        let (report, messages) = self.summarize(Trigger::Overflow, cancel).await;
        accounting.model_requests = accounting.model_requests.saturating_add(report.requests);
        accounting.usage.add(&report.usage);
        if let Some(error) = report.persistence_error {
            return Err(RunStop::error(error));
        }
        if messages.is_some() {
            return Ok(());
        }
        if report.outcome == compaction::Outcome::Aborted {
            return Err(RunStop::aborted());
        }
        let reason = report.error.unwrap_or_else(|| "compaction made no progress".into());
        Err(RunStop::error(format!(
            "{error}; compacting the context to recover failed: {reason}"
        )))
    }

    /// Announce a retry, omit the failed answer from the context and wait out the backoff.
    /// Ends the run when the session cannot record the edit or the wait is cancelled.
    pub async fn schedule_retry(
        &mut self,
        retry: &RetryPolicy,
        accounting: &mut RunAccounting,
        failed: &AssistantMessage,
        failed_entry: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<(), RunStop> {
        accounting.retries += 1;
        let delay = retry.delay_for(accounting.retries, failed);
        let scheduled = AgentEvent::RetryScheduled {
            attempt: accounting.retries,
            max_retries: retry.max_retries,
            delay_ms: duration_ms(delay),
            error: failed.error_message.clone().unwrap_or_default(),
        };
        self.hooks.emit(scheduled, self.to).await;
        if let Some(target) = failed_entry {
            self.omit_failed_attempt(target).await.map_err(RunStop::error)?;
        }
        tokio::select! {
            () = tokio::time::sleep(delay) => Ok(()),
            () = cancel.cancelled() => Err(RunStop::aborted()),
        }
    }
}

#[async_trait]
impl SessionHost for RunHost<'_> {
    async fn append_message(&mut self, message: &Message) -> Result<String, String> {
        // The event that reports the message owns it, so the session stores a copy.
        self.session
            .append_message(message.clone())
            .await
            .map_err(|error| format!("cannot persist session entry: {error:#}"))
    }

    async fn edit_context(&mut self, index: usize, replacement: Option<&str>) -> Result<String, String> {
        let projection = self.session.projection();
        let target = projection
            .get(index)
            .ok_or_else(|| format!("context index {index} is out of bounds"))?;
        self.session
            .append_context_edit(&target.entry_id, replacement.map(str::to_string))
            .await
            .map_err(|error| format!("cannot persist context edit: {error:#}"))
    }

    async fn record_request(&mut self, request: &Request, provider: &dyn Provider) -> Result<String, String> {
        let options = provider.request_options();
        self.session
            .record_request(request, provider.name(), options, RequestSource::Projection)
            .await
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
        let Some(limit) = self.compaction.trigger_bytes else {
            return ContextPreparation::unchanged();
        };
        match compaction::context_bytes(&self.session.messages()) {
            Ok(bytes) if bytes <= limit => return ContextPreparation::unchanged(),
            Ok(_) => {}
            Err(error) => return ContextPreparation::failed(format!("cannot measure context: {error:#}")),
        }
        if let Some(reason) = self.pause.reason() {
            return ContextPreparation::failed(format!(
                "automatic compaction is paused because it failed earlier in this session ({reason}); \
                 compact manually to retry, start a new session, or disable automatic compaction"
            ));
        }
        let (report, messages) = self.summarize(Trigger::Automatic, cancel).await;
        let result = if let Some(error) = report.persistence_error {
            Prepared::Unrecorded(error)
        } else if let Some(messages) = messages {
            Prepared::Updated(messages)
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
