//! What the loop needs from the session it runs in. The Runner implements it over its session
//! store; a run without a session gets [`NoSession`], which records nothing, so `run_loop`
//! keeps everything in its in-memory context.

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::ai::{Message, Provider, Request, ToolCall, Usage, UserMessageKind, now_ms};
use crate::session::CustomMessage;

/// Records what the loop does, before the loop acts on it. When the session cannot record, a
/// method returns an error (`prepare_context`: [`Prepared::Unrecorded`]) and the loop stops at
/// once, because an unattended run must not keep acting without an audit trail. A host
/// without a session records nothing and returns no entry IDs.
#[async_trait]
pub(crate) trait SessionHost: Send + Sync {
    /// Record a finished message. Returns its entry ID.
    async fn append_message(&mut self, message: &Message) -> Result<Option<String>, String>;

    /// Record a message from an extension. Returns its entry ID and the message as the
    /// projection holds it, which is what the loop keeps in its context.
    async fn append_custom_message(&mut self, message: &CustomMessage) -> Result<(Option<String>, Message), String>;

    /// Record state of an extension. Returns its entry ID.
    async fn append_custom_entry(&mut self, custom_type: &str, data: Value) -> Result<Option<String>, String>;

    /// Record an edit of the message at `index` in the current projection, before the loop
    /// applies it: `None` omits the message, `Some` replaces its text. Returns the edit's
    /// entry ID.
    async fn edit_context(&mut self, index: usize, replacement: Option<&str>) -> Result<Option<String>, String>;

    /// Record a request before it is sent. Returns the ID of its `request_started` entry.
    async fn record_request(&mut self, request: &Request, provider: &dyn Provider) -> Result<Option<String>, String>;

    /// Record that a call with side effects is about to run, after its checks and approval.
    async fn record_tool_call_started(&mut self, call: &ToolCall) -> Result<(), String>;

    /// Prepare the context before a task request, for example by summarizing it. A
    /// replacement is already recorded; request costs, failed attempts included, count
    /// against the run's budget.
    async fn prepare_context(&mut self, cancel: &CancellationToken) -> ContextPreparation;
}

/// The host of a run without a session: nothing is recorded, so no step can fail to be, and
/// the loop keeps messages and edits only in its context.
pub(crate) struct NoSession;

#[async_trait]
impl SessionHost for NoSession {
    async fn append_message(&mut self, _message: &Message) -> Result<Option<String>, String> {
        Ok(None)
    }

    async fn append_custom_message(&mut self, message: &CustomMessage) -> Result<(Option<String>, Message), String> {
        let projected = Message::User {
            content: message.content.clone(),
            timestamp: now_ms(),
            kind: UserMessageKind::Custom,
        };
        Ok((None, projected))
    }

    async fn append_custom_entry(&mut self, _custom_type: &str, _data: Value) -> Result<Option<String>, String> {
        Ok(None)
    }

    async fn edit_context(&mut self, _index: usize, _replacement: Option<&str>) -> Result<Option<String>, String> {
        Ok(None)
    }

    async fn record_request(&mut self, _request: &Request, _provider: &dyn Provider) -> Result<Option<String>, String> {
        Ok(None)
    }

    async fn record_tool_call_started(&mut self, _call: &ToolCall) -> Result<(), String> {
        Ok(())
    }

    async fn prepare_context(&mut self, _cancel: &CancellationToken) -> ContextPreparation {
        ContextPreparation::unchanged()
    }
}

/// What preparing the context cost and produced. The loop knows neither profiles nor
/// compaction policy; the cost counts against the run's budget whatever the result.
pub(crate) struct ContextPreparation {
    pub requests: u32,
    pub usage: Usage,
    pub result: Prepared,
}

impl ContextPreparation {
    /// No request was made and the context stays as it is.
    pub(crate) fn unchanged() -> Self {
        Self {
            requests: 0,
            usage: Usage::default(),
            result: Prepared::Unchanged,
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

/// How the context looks after preparation.
pub(crate) enum Prepared {
    Unchanged,
    /// The context was replaced, by a summary for example.
    Updated(Vec<Message>),
    /// Preparation failed; the context is unchanged.
    Failed(String),
    /// The session could not record the preparation, so the run must stop.
    Unrecorded(String),
}
