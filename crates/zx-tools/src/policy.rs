//! The built-in tool-call policies: rules by tool or command prefix, the command policy of
//! the `bash` tool, and the auto-approval of plain commands; the loop sees them only through
//! Hooks.
//!
//! - `rules`: rules by tool name or bash command prefix
//! - `command`: the command policy of `bash`: denied patterns and programs, and the allowlist
//! - `auto_approve`: the bash commands that run without asking where a rule would ask

mod auto_approve;
mod command;
mod rules;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

pub use auto_approve::{AutoApprove, ProgramOptions};
pub use command::{CommandPolicy, DEFAULT_DENIED_PATTERNS, DENIED_PROGRAMS};
pub use rules::{Rule, RuleMatcher, RulePolicy};

use crate::agent::{Decision, HookContext, HookResult, HookStage, Hooks, ToolPolicy};
use crate::ai::ToolCall;

/// Adapts an existing asynchronous policy to the bounded before_tool hook.
pub struct PolicyHook {
    policy: Arc<dyn ToolPolicy>,
}

impl PolicyHook {
    /// The hook that asks `policy` about every tool call.
    pub fn new(policy: Arc<dyn ToolPolicy>) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl Hooks for PolicyHook {
    /// Before any extension sees the call.
    fn stage(&self) -> HookStage {
        HookStage::Policy
    }

    async fn before_tool(&self, call: &ToolCall, _ctx: &HookContext) -> HookResult<Decision> {
        Ok(self.policy.check(&call.name, &call.arguments).await)
    }
}

/// The `command` argument of a bash call, when it is a string.
fn command_of(args: &Value) -> Option<&str> {
    args.get("command").and_then(Value::as_str)
}

/// Whether a normalized pipeline segment starts with `prefix` at a word boundary.
pub(super) fn begins_with(segment: &str, prefix: &str) -> bool {
    segment
        .strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
}

pub(super) fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

pub(super) fn normalize_all(texts: impl IntoIterator<Item = impl AsRef<str>>) -> Vec<String> {
    texts
        .into_iter()
        .map(|text| normalize(text.as_ref()))
        .filter(|text| !text.is_empty())
        .collect()
}
