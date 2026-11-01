//! Instructions for the interactive terminal frontend; runtime defaults stay neutral.

/// General conversation instructions. Terminal approvals are enforced by host code.
pub const INSTRUCTIONS: &str = include_str!("../../profiles/assistant.md");
