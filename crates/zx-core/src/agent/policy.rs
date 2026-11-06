//! Tool-call policy (pi: `beforeToolCall`, wired to the extension `tool_call` event).
//!
//! An unattended run has no one to ask, so a policy decides on its own and should deny
//! when unsure. Policies are a first line of defense, not a security boundary: run the
//! agent as a least-privilege user, in a container or VM, for real isolation.
//!
//! Extensions decide through [`Hooks::before_tool`](super::Hooks::before_tool); a
//! [`ToolPolicy`] is how the built-in policies join, through `policy::PolicyHook`. The
//! concrete policies live outside the kernel, in the zx-tools crate (`zx_agent::policy`):
//!
//! ```compile_fail
//! use zx_core::agent::CommandPolicy;
//! ```

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The verdict on one tool call. In JSON: `{"decision":"allow"}`, or `deny` / `ask` with a
/// `reason`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", content = "reason", rename_all = "snake_case")]
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn allow_all_allows_every_call_whatever_its_arguments() {
        let decision = AllowAll.check("bash", &json!({"command": "rm -rf /"})).await;
        assert_eq!(decision, Decision::Allow);
        assert_eq!(AllowAll.check("unknown", &json!(null)).await, Decision::Allow);
    }
}
