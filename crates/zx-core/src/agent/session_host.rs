//! What the loop needs from the session it runs in: a place to record each step before acting
//! on it, the projection it sends the model, and a chance to prepare that context before a
//! request. A [`SessionStore`] is a host on its own; the Runner's host adds summaries.

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{AgentEvent, EventTarget, LoopConfig};
use crate::ai::{Message, Provider, Request, ToolCall, Usage};
use crate::session::{CustomMessage, Entry, RequestSource, SessionStore};

/// Records what the loop does, before the loop acts on it, and gives the loop the context it
/// sends: the session's projection. When the session cannot record, a method returns an error
/// (`prepare_context`: [`Prepared::Unrecorded`]) and the loop stops at once, because an
/// unattended run must not keep acting without an audit trail.
#[async_trait]
pub(crate) trait SessionHost: Send + Sync {
    /// The messages the model sees now: the session's projection.
    fn messages(&self) -> Vec<Message>;

    /// Record a finished message. Returns its entry ID.
    async fn append_message(&mut self, message: &Message) -> Result<String>;

    /// Record a message from an extension. Returns its entry ID and the message as the
    /// projection holds it.
    async fn append_custom_message(&mut self, message: &CustomMessage) -> Result<(String, Message)>;

    /// Record state of an extension. Returns its entry ID.
    async fn append_custom_entry(&mut self, custom_type: &str, data: Value) -> Result<String>;

    /// Record an edit of the message at `index` in the current projection: `None` omits the
    /// message, `Some` replaces its text. Returns the edit's entry ID.
    async fn edit_context(&mut self, index: usize, replacement: Option<&str>) -> Result<String>;

    /// Record a request before it is sent. Returns the ID of its `request_started` entry.
    async fn record_request(&mut self, request: &Request, provider: &dyn Provider) -> Result<String>;

    /// Record that a call with side effects is about to run, after its checks and approval.
    async fn record_tool_call_started(&mut self, call: &ToolCall) -> Result<()>;

    /// Prepare the context before a task request, for example by summarizing it; what it
    /// records is in the projection afterwards. Request costs, failed attempts included,
    /// count against the run's budget.
    async fn prepare_context(&mut self, cancel: &CancellationToken) -> ContextPreparation;
}

/// The context of the error for an entry the session could not record.
pub(crate) const PERSIST_FAILED: &str = "cannot persist session entry";

/// Record the custom entries extensions asked for since the last turn (pi: `appendEntry`),
/// each announced as a `custom_entry` event. An error means the session could not record one.
pub(crate) async fn record_pending_entries(
    config: &LoopConfig,
    session: &mut dyn SessionHost,
    target: EventTarget<'_>,
) -> Result<()> {
    for entry in config.queues.take_entries() {
        let entry_id = session.append_custom_entry(&entry.custom_type, entry.data).await?;
        let event = AgentEvent::CustomEntry {
            entry_id,
            custom_type: entry.custom_type,
        };
        config.hooks.emit(event, target).await;
    }
    Ok(())
}

/// A session on its own: every step is recorded in the store, and the context is never
/// prepared, as no one summarizes it.
#[async_trait]
impl SessionHost for SessionStore {
    fn messages(&self) -> Vec<Message> {
        SessionStore::messages(self)
    }

    async fn append_message(&mut self, message: &Message) -> Result<String> {
        // The event that reports the message owns it, so the session stores a copy.
        SessionStore::append_message(self, message.clone())
            .await
            .context(PERSIST_FAILED)
    }

    async fn append_custom_message(&mut self, message: &CustomMessage) -> Result<(String, Message)> {
        let id = SessionStore::append_custom_message(self, message.clone())
            .await
            .context(PERSIST_FAILED)?;
        match self.get(&id) {
            Some(Entry::CustomMessage(entry)) => Ok((id, entry.projected())),
            _ => bail!("custom message {id} was not recorded"),
        }
    }

    async fn append_custom_entry(&mut self, custom_type: &str, data: Value) -> Result<String> {
        SessionStore::append_custom_entry(self, custom_type, data)
            .await
            .context(PERSIST_FAILED)
    }

    async fn edit_context(&mut self, index: usize, replacement: Option<&str>) -> Result<String> {
        let projection = self.projection();
        let target = projection
            .get(index)
            .ok_or_else(|| anyhow!("context index {index} is out of bounds"))?;
        self.append_context_edit(&target.entry_id, replacement.map(str::to_string))
            .await
            .context("cannot persist context edit")
    }

    async fn record_request(&mut self, request: &Request, provider: &dyn Provider) -> Result<String> {
        let options = provider.request_options();
        SessionStore::record_request(self, request, provider.name(), options, RequestSource::Projection)
            .await
            .context("cannot persist request checkpoint")
    }

    async fn record_tool_call_started(&mut self, call: &ToolCall) -> Result<()> {
        self.append_tool_call_started(&call.id, &call.name)
            .await
            .map(|_| ())
            .with_context(|| format!("cannot persist the start of tool call {:?}", call.id))
    }

    async fn prepare_context(&mut self, _cancel: &CancellationToken) -> ContextPreparation {
        ContextPreparation::ready()
    }
}

/// What preparing the context cost and how it ended. The loop knows neither profiles nor
/// compaction policy; the cost counts against the run's budget whatever the result.
pub(crate) struct ContextPreparation {
    pub requests: u32,
    pub usage: Usage,
    pub result: Prepared,
}

impl ContextPreparation {
    /// No request was made; the context is ready as it is.
    pub(crate) fn ready() -> Self {
        Self {
            requests: 0,
            usage: Usage::default(),
            result: Prepared::Ready,
        }
    }

    /// Preparation failed before any request was made.
    pub(crate) fn failed(error: String) -> Self {
        Self {
            requests: 0,
            usage: Usage::default(),
            result: Prepared::Failed(error),
        }
    }
}

/// How preparing the context ended.
pub(crate) enum Prepared {
    /// The projection is the context to send, summarized or not.
    Ready,
    /// Preparation failed; the context is unchanged.
    Failed(String),
    /// The session could not record the preparation, so the run must stop.
    Unrecorded(anyhow::Error),
}
