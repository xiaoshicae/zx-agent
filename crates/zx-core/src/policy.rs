//! Whether a tool call may run: the [`ToolPolicy`] interface, the decision it gives, and the
//! approval of a call a policy asks about (pi: `beforeToolCall`, wired to the extension
//! `tool_call` event).
//!
//! An unattended run has no one to ask, so a policy should deny when unsure. A decision is a
//! first line of defense, not a security boundary: run the agent as a least-privilege user,
//! in a container or VM, for real isolation.
//!
//! Only policies decide; hooks observe and transform. The policies that ship with zx-agent
//! live outside the kernel, in the zx-builtin crate (`zx_agent::builtin::policy`):
//!
//! ```compile_fail
//! use zx_core::policy::CommandPolicy;
//! ```
//!
//! - `chain`: the policies of a loop, and how their decisions combine
//! - `approval`: who answers when a policy asks, the answer, and the gate where the call
//!   waits for it

mod approval;
mod chain;

use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::ai::ToolCall;
use crate::guard::CancelSignal;

pub use approval::{ApprovalDecision, ApprovalMode};
pub(crate) use approval::{ApprovalGate, NO_HUMAN, RequestEnd, request_id};
pub use chain::{DEFAULT_POLICY_TIMEOUT, PolicyChain};

/// Decides whether a tool call may run. Registered in a loop's [`PolicyChain`], where the
/// strictest decision of all policies stands.
///
/// `check` is called after the tool is found and its arguments are validated, before it
/// runs; the call cannot be changed here. Return [`PolicyDecision::Deny`] when unsure. A
/// check that panics, outlasts its timeout or is cut short by cancellation denies the call,
/// with a reason that names the policy.
#[async_trait]
pub trait ToolPolicy: Send + Sync {
    async fn check(&self, call: &ToolCall, ctx: &PolicyContext) -> PolicyDecision;
}

/// What a policy knows about the run a call belongs to. Fields are only ever added, so the
/// struct is `#[non_exhaustive]`: read its fields, and outside the crate build one with
/// [`PolicyContext::new`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct PolicyContext {
    /// The ID of the session the run belongs to.
    pub session_id: String,
    /// The run, as in the events' `runId`.
    pub run_id: String,
    /// The tools' working directory.
    pub cwd: PathBuf,
    /// The session's labels: what its host said about it when it was created, such as its
    /// tenant or the chat it belongs to.
    pub labels: BTreeMap<String, String>,
    /// Who sent the input the run is answering: the actor of the latest prompt of the
    /// session ([`Input::User`](crate::input::Input::User)), `None` when the host named
    /// nobody. Other people may have spoken earlier in the run: a policy that must hold for
    /// each of them keeps track of them itself, with a hook's `on_input`.
    pub actor: Option<String>,
    /// Set when the run is cancelled; the chain then stops waiting for the policy.
    pub cancel: CancelSignal,
}

impl PolicyContext {
    /// A context for a new run of session `session_id` with a fresh ID, cancelled with
    /// `cancel`, without labels or an actor.
    pub fn new(session_id: String, cwd: PathBuf, cancel: CancellationToken) -> Self {
        Self {
            session_id,
            run_id: uuid::Uuid::new_v4().to_string(),
            cwd,
            labels: BTreeMap::new(),
            actor: None,
            cancel: CancelSignal::new(cancel),
        }
    }
}

/// The decision on one tool call. In JSON: `{"decision":"allow"}`, or `deny` / `ask` with a
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

impl PolicyDecision {
    /// The decision as JSON names it, without its reason: `allow`, `deny` or `ask`.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny(_) => "deny",
            Self::Ask(_) => "ask",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_the_ones_json_gives() {
        for decision in [
            PolicyDecision::Allow,
            PolicyDecision::Deny("no".into()),
            PolicyDecision::Ask("why".into()),
        ] {
            assert_eq!(serde_json::to_value(&decision).unwrap()["decision"], decision.name());
        }
    }
}
