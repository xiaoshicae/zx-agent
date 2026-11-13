//! The built-in tool-call policies, each a [`ToolPolicy`](zx_core::policy::ToolPolicy): rules by
//! tool, the command policy of the `bash` tool, and the auto-approval of plain commands.
//!
//! - `rules`: rules by tool name
//! - `command`: the command policy of `bash`: denied patterns and programs
//! - `auto_approve`: the bash commands that run without asking where a rule would ask

mod auto_approve;
mod command;
mod rules;

use serde_json::Value;

pub use auto_approve::{AutoApprove, ProgramOptions};
pub use command::{CommandPolicy, DEFAULT_DENIED_PATTERNS, DENIED_PROGRAMS};
pub use rules::{Rule, RuleMatcher, RulePolicy};

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
