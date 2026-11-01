//! What the loop needs from the session it runs in. The Runner implements it over its session
//! store; without a host, `run_loop` keeps everything in its in-memory context.

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::ai::{Message, Provider, Request, ToolCall, Usage};

/// Records what the loop does, before the loop acts on it. When the session cannot record, a
/// method returns an error (`prepare_context`: [`ContextPreparation::Unrecorded`]) and the loop
/// stops at once, because an unattended run must not keep acting without an audit trail.
#[async_trait]
pub(crate) trait SessionHost: Send + Sync {
    /// Record a finished message. Returns its entry ID.
    async fn append_message(&mut self, message: &Message) -> Result<String, String>;

    /// Record an edit of the message at `index` in the current projection, before the loop
    /// applies it: `None` omits the message, `Some` replaces its text.
    async fn edit_context(&mut self, index: usize, replacement: Option<&str>) -> Result<(), String>;

    /// Record a request before it is sent.
    async fn record_request(&mut self, request: &Request, provider: &dyn Provider) -> Result<(), String>;

    /// Record that a call with side effects is about to run, after its checks and approval.
    async fn record_tool_call_started(&mut self, call: &ToolCall) -> Result<(), String>;

    /// Prepare the context before a task request, for example by summarizing it. A
    /// replacement is already recorded; request costs, failed attempts included, count
    /// against the run's budget.
    async fn prepare_context(&mut self, cancel: &CancellationToken) -> ContextPreparation;
}

/// The result of preparing the context. The loop knows neither profiles nor compaction policy.
pub(crate) enum ContextPreparation {
    Unchanged,
    Updated {
        messages: Vec<Message>,
        requests: u32,
        usage: Usage,
    },
    /// Preparation failed; the context is unchanged.
    Failed {
        error: String,
        requests: u32,
        usage: Usage,
    },
    /// The session could not record the preparation, so the run must stop.
    Unrecorded {
        error: String,
        requests: u32,
        usage: Usage,
    },
}
