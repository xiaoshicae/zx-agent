//! Built-in scenario policies for SDK callers, who select them explicitly.
//!
//! The CLI reads its defaults from the built-in profiles instead (`profiles/*.toml`), which
//! share these instructions; a profile test keeps the limits and continuations equal. Prompt
//! assembly and the agent runtime do not depend on these presets.

pub mod assistant;
pub mod troubleshoot;
