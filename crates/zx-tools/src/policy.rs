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

/// Whether a pipeline segment starts with `prefix` at a word boundary. Both have their
/// whitespace collapsed and keep their case.
pub(super) fn begins_with(segment: &str, prefix: &str) -> bool {
    segment
        .strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
}

/// `text` with every run of whitespace as one space and none at either end.
pub(super) fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `text` as the texts that deny a command or keep it asking are compared: whitespace
/// collapsed and lowercased. Ignoring case there only catches more commands; what lets a
/// command run is compared in its own case (see `AutoApprove`).
pub(super) fn normalize(text: &str) -> String {
    collapse_whitespace(text).to_lowercase()
}

/// Each of `texts` through `form`, without those that come out empty.
pub(super) fn non_empty(texts: impl IntoIterator<Item = impl AsRef<str>>, form: fn(&str) -> String) -> Vec<String> {
    texts
        .into_iter()
        .map(|text| form(text.as_ref()))
        .filter(|text| !text.is_empty())
        .collect()
}
