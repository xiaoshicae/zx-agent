//! What the tests of the policies share.

use serde_json::Value;
use zx_core::policy::PolicyDecision;

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

/// Whether `decision` denies the call.
pub(super) fn denied(decision: &PolicyDecision) -> bool {
    matches!(decision, PolicyDecision::Deny(_))
}
