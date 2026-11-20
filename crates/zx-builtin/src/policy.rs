//! The built-in tool-call policies, each a [`ToolPolicy`](zx_core::policy::ToolPolicy): rules
//! by tool, and the command policy of the `bash` tool; with them [`AutoApprove`], the bash
//! commands that a [`RulePolicy`] lets run where one of its rules asks.
//!
//! - `rules`: rules by tool name
//! - `command`: the command policy of `bash`: denied patterns and programs
//! - `auto_approve`: the bash commands that run without asking where a rule would ask
//! - `test_support`, compiled for tests only: the calls the tests of the policies check

mod auto_approve;
mod command;
mod rules;
#[cfg(test)]
mod test_support;

use serde_json::Value;

pub use self::auto_approve::{AutoApprove, ProgramOptions};
pub use self::command::{CommandPolicy, DEFAULT_DENIED_PATTERNS, DEFAULT_DENIED_PROGRAMS, DEFAULT_READ_ONLY_PROGRAMS};
pub use self::rules::{Rule, RuleMatcher, RulePolicy};
use crate::ai::ToolCall;
use crate::tools::BASH;

/// A tool call, as the policies that read bash commands see it.
enum BashCall<'a> {
    /// A call of another tool.
    Other,
    /// A call of bash whose `command` argument is missing or not a string.
    NoCommand,
    /// A call of bash, with its command.
    Command(&'a str),
}

/// What `call` is to the policies that read bash commands: whether it calls bash, and with
/// what command.
fn bash_call(call: &ToolCall) -> BashCall<'_> {
    if call.name != BASH {
        return BashCall::Other;
    }
    match call.arguments.get("command").and_then(Value::as_str) {
        Some(command) => BashCall::Command(command),
        None => BashCall::NoCommand,
    }
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
