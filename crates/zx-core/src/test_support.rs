//! What the unit tests of several of the kernel's modules share.

use serde_json::Value;

use crate::ai::ToolCall;

/// The call `c` of the tool `name` with `arguments`.
pub(crate) fn tool_call(name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: "c".into(),
        name: name.into(),
        arguments,
        arguments_error: None,
    }
}
