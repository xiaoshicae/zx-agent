//! zx-agent: a minimal, general-purpose agent kernel for interactive, unattended, and
//! embedded use. Scenario choices such as prompts and limits belong outside the kernel; the
//! plan is in docs/ROADMAP.md.
//!
//! Layering follows pi (<https://github.com/earendil-works/pi>):
//! - [`ai`]: message types, the streaming event protocol, providers (pi: packages/ai)
//! - [`agent`]: the agent loop, tool execution, and tool policy (pi: packages/agent)
//! - [`session`]: append-only JSONL session tree and its projection (pi: SessionManager)
//! - [`runner`]: retries, limits, and persistence around the loop (pi: AgentSession, subset)
//! - [`presets`]: opt-in scenario instructions and limits
//! - [`prompt`]: system prompt assembly and the generated environment section
//! - [`tools`]: built-in tools

pub mod agent;
pub mod ai;
pub mod presets;
pub mod prompt;
pub mod runner;
pub mod session;
mod sync;
pub mod tools;
