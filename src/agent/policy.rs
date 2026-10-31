//! Tool-call policy (pi: `beforeToolCall`, wired to the extension `tool_call` event).
//!
//! An unattended run has no one to ask, so a policy decides on its own and should deny
//! when unsure. Policies are a first line of defense, not a security boundary: run the
//! agent as a least-privilege user, in a container or VM, for real isolation.

use async_trait::async_trait;
use serde_json::Value;

/// The verdict on one tool call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// The reason is shown to the model as the tool result.
    Deny(String),
    /// Ask the host; no human is an explicit rejection.
    Ask(String),
}

/// Decides whether a tool call may run.
#[async_trait]
pub trait ToolPolicy: Send + Sync {
    /// Called after the tool is found and its arguments parsed, before it runs.
    async fn check(&self, tool_name: &str, args: &Value) -> Decision;
}

/// Policy that allows every call. For tests and fully trusted environments.
pub struct AllowAll;

#[async_trait]
impl ToolPolicy for AllowAll {
    async fn check(&self, _tool_name: &str, _args: &Value) -> Decision {
        Decision::Allow
    }
}

// Compatibility exports; concrete policies live outside the loop.
pub use crate::policy::{CommandPolicy, DEFAULT_DENIED_PATTERNS};
