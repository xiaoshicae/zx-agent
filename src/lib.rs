//! zx-agent: a minimal, general-purpose agent kernel for interactive, unattended, and
//! embedded use. Scenario choices such as prompts and limits belong outside the kernel; the
//! plan is in docs/ROADMAP.md.
//!
//! Layering follows pi (<https://github.com/earendil-works/pi>):
//! - [`ai`]: message types, the streaming event protocol, providers (pi: packages/ai)
//! - [`agent`]: the agent loop, tool execution, and tool policy (pi: packages/agent)
//! - [`session`]: append-only JSONL session tree and its projection (pi: SessionManager)
//! - [`agent_session`]: shared prompts, cancellation, subscriptions and model switching
//! - [`runner`]: exclusive run ownership, retries, limits and persistence
//! - [`presets`]: opt-in scenario instructions and limits
//! - [`prompt`]: system prompt assembly and the generated environment section
//! - [`tools`]: built-in tools

pub mod agent;
pub mod agent_session;
pub mod ai;
pub mod compaction;
pub mod extensions;
pub mod interaction;
pub mod output_store;
pub mod policy;
pub mod presets;
pub mod profile;
pub mod prompt;
pub mod runner;
pub mod session;
mod sync;
pub mod tools;

pub use agent_session::AgentSession;
