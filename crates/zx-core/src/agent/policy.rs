//! The verdict on a tool call (pi: `beforeToolCall`, wired to the extension `tool_call` event).
//!
//! An unattended run has no one to ask, so whatever decides should deny when unsure. A
//! verdict is a first line of defense, not a security boundary: run the agent as a
//! least-privilege user, in a container or VM, for real isolation.
//!
//! Hooks decide through [`Hooks::before_tool`](super::Hooks::before_tool). The policies that
//! ship with zx-agent, and the trait they implement, live outside the kernel, in the zx-tools
//! crate (`zx_agent::policy`):
//!
//! ```compile_fail
//! use zx_core::agent::CommandPolicy;
//! ```

use serde::{Deserialize, Serialize};

/// The verdict on one tool call. In JSON: `{"decision":"allow"}`, or `deny` / `ask` with a
/// `reason`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", content = "reason", rename_all = "snake_case")]
pub enum PolicyDecision {
    Allow,
    /// The reason is shown to the model as the tool result.
    Deny(String),
    /// Ask the host; no human is an explicit rejection.
    Ask(String),
}
