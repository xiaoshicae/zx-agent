//! What a run lends the loop and the summary host: the session it records in, the hooks and
//! sink it reports to, and the model it summarizes with. The Runner builds one per run and
//! the loop reaches it as its [`SessionHost`] (pi: AgentSession writes on `message_end`).

use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::accounting::RunAccounting;
use crate::agent::{
    AgentEvent, ContextPreparation, Emit, HookChain, LoopConfig, LoopOutcome, MessageQueues, Prepared, QueuedMessage,
    SessionHost,
};
use crate::ai::retry::RetryPolicy;
use crate::ai::{AssistantMessage, Message, ModelMeta, Provider, Request, StopReason, ToolCall, duration_ms};
use crate::compaction::{self, Trigger};
use crate::session::{CustomMessage, Entry, RequestSource, SessionStore};

/// How a run ends when it stops before a finished answer.
pub(super) struct RunStop {
    pub outcome: LoopOutcome,
    /// The reason, for [`LoopOutcome::Error`]; the other outcomes explain themselves.
    pub error: Option<String>,
}

impl RunStop {
    pub(super) fn error(reason: impl Into<String>) -> Self {
        Self {
            outcome: LoopOutcome::Error,
            error: Some(reason.into()),
        }
    }

    pub(super) fn aborted() -> Self {
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
    /// Prices by model, for the cost of summary responses.
    pub models: &'a HashMap<String, ModelMeta>,
}

impl<'a> RunHost<'a> {
    pub(super) fn new(
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
            models: &config.models,
        }
    }

    /// Summarize the old prefix with `trigger`; the pause follows the outcome. When the
    /// report says `Completed`, the session's projection is the new context.
    pub(super) async fn summarize(&mut self, trigger: Trigger, cancel: &CancellationToken) -> compaction::Report {
        let mut host = compaction::Host {
            session: &mut *self.session,
            provider: self.provider,
            model: self.model,
            config: self.compaction,
            to: self.to,
            hooks: self.hooks,
            attempt: self.attempt,
            models: self.models,
        };
        let report = host.run(trigger, cancel).await;
        self.pause.note(&report);
        report
    }

    /// The input a continuation starts with, after omitting a failed answer at the end; an
    /// error when there is nothing for the model to answer.
    pub(super) async fn continuation(&mut self, queues: &MessageQueues) -> Result<Vec<QueuedMessage>, String> {
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

    /// Record the custom entries that hooks queued and the loop has not written (pi:
    /// `appendEntry`), each announced as a `custom_entry` event.
    pub(super) async fn record_pending_entries(&mut self, queues: &MessageQueues) -> Result<(), String> {
        for entry in queues.take_entries() {
            let id = self
                .session
                .append_custom_entry(&entry.custom_type, entry.data)
                .await
                .map_err(|error| format!("cannot persist session entry: {error:#}"))?;
            let event = AgentEvent::CustomEntry {
                entry_id: Some(id),
                custom_type: entry.custom_type,
            };
            self.hooks.emit(event, self.to).await;
        }
        Ok(())
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
    /// from the compacted context. The summary's cost is
    /// added to `accounting` whether or not it succeeds.
    pub(super) async fn recover_overflow(
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
        let report = self.summarize(Trigger::Overflow, cancel).await;
        accounting.model_requests = accounting.model_requests.saturating_add(report.requests);
        accounting.usage.add(&report.usage);
        if let Some(error) = report.persistence_error {
            return Err(RunStop::error(error));
        }
        if report.outcome == compaction::Outcome::Completed {
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

    async fn append_custom_message(&mut self, message: &CustomMessage) -> Result<(String, Message), String> {
        let id = self
            .session
            .append_custom_message(message.clone())
            .await
            .map_err(|error| format!("cannot persist session entry: {error:#}"))?;
        match self.session.get(&id) {
            Some(Entry::CustomMessage(entry)) => Ok((id, entry.projected())),
            _ => Err(format!("custom message {id} was not recorded")),
        }
    }

    async fn append_custom_entry(&mut self, custom_type: &str, data: Value) -> Result<String, String> {
        self.session
            .append_custom_entry(custom_type, data)
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
