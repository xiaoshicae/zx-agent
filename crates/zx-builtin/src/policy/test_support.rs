//! What the tests of the policies share.

use serde_json::Value;

use crate::ai::ToolCall;

/// A call of the tool `name` with `arguments`.
pub(super) fn call(name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: "c1".into(),
        name: name.into(),
        arguments,
        arguments_error: None,
    }
}
