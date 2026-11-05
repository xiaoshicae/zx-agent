//! Bridge the loop's session operations to the Runner's session store.

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::agent::{ContextPreparation, Emit, HookChain, SessionHost};
use crate::ai::{Message, Provider, Request, ToolCall, Usage};
use crate::compaction::{self, Trigger};
use crate::session::{RequestSource, SessionStore};

/// Records each step of a run in the session before the loop acts on it (pi: AgentSession
/// writes on `message_end`), and prepares the context before each task request. A failed
/// write stops the run: an unattended run must not keep acting without an audit trail.
pub(super) struct RunHost<'a> {
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
            return ContextPreparation::Unchanged;
        };
        match compaction::context_bytes(&self.session.messages()) {
            Ok(bytes) if bytes <= limit => return ContextPreparation::Unchanged,
            Ok(_) => {}
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
        };
        let (report, messages) = host.run(Trigger::Automatic, cancel).await;
        if let Some(error) = report.persistence_error {
            return ContextPreparation::Unrecorded {
                error,
                requests: report.requests,
                usage: report.usage,
            };
        }
        if let Some(messages) = messages {
            ContextPreparation::Updated {
                messages,
                requests: report.requests,
                usage: report.usage,
            }
        } else {
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
