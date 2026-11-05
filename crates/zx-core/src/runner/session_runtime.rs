//! Bridge the loop's session operations to the Runner's session store.

use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::agent::{ContextPreparation, Emit, HookChain, SessionRuntime};
use crate::ai::{Message, ModelMeta, Provider, Request, ToolCall, Usage};
use crate::compaction::{self, Trigger};
use crate::session::{CustomMessage, Entry, RequestSource, SessionStore};

/// Records each step of a run in the session before the loop acts on it (pi: AgentSession
/// writes on `message_end`), and prepares the context before each task request. A failed
/// write stops the run: an unattended run must not keep acting without an audit trail.
pub(super) struct RunnerSessionRuntime<'a> {
    pub(super) session: &'a mut SessionStore,
    pub(super) to: Emit<'a>,
    pub(super) hooks: &'a HookChain,
    pub(super) provider: &'a dyn Provider,
    pub(super) model: &'a str,
    pub(super) compaction: &'a compaction::Config,
    /// Borrowed from the Runner, so the pause outlives one prompt and a dropped run.
    pub(super) compaction_pause: &'a mut Option<String>,
    /// The attempt this host serves, for the events of an automatic compaction.
    pub(super) attempt: u32,
    /// Prices by model, for the cost of summary responses.
    pub(super) models: &'a HashMap<String, ModelMeta>,
}

#[async_trait]
impl SessionRuntime for RunnerSessionRuntime<'_> {
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
            return ContextPreparation::Unchanged;
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
            Ok(false) => return ContextPreparation::Unchanged,
            Ok(true) => {}
            Err(error) => {
                return ContextPreparation::Failed {
                    error: format!("cannot measure context: {error:#}"),
                    requests: 0,
                    usage: Usage::default(),
                };
            }
        }
        if let Some(reason) = self.compaction_pause.as_deref() {
            return ContextPreparation::Failed {
                error: format!(
                    "automatic compaction is paused because it failed earlier in this session ({reason}); \
                     compact manually to retry, start a new session, or disable automatic compaction"
                ),
                requests: 0,
                usage: Usage::default(),
            };
        }
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
        let (report, messages) = host.run(Trigger::Automatic, cancel).await;
        if let Some(error) = report.persistence_error {
            return ContextPreparation::Unrecorded {
                error,
                requests: report.requests,
                usage: report.usage,
            };
        }
        match messages {
            Some(messages) => ContextPreparation::Updated {
                messages,
                requests: report.requests,
                usage: report.usage,
            },
            None => {
                let error = report
                    .error
                    .unwrap_or_else(|| "automatic compaction made no progress".into());
                // A paid summary request failed; repeating it on every prompt would keep
                // paying for the same failure. Cancellation is the caller's choice.
                if report.outcome == compaction::Outcome::Failed && report.requests > 0 {
                    *self.compaction_pause = Some(error.clone());
                }
                ContextPreparation::Failed {
                    error,
                    requests: report.requests,
                    usage: report.usage,
                }
            }
        }
    }
}
