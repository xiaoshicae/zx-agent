//! zx-agent: a minimal agent kernel for unattended automation.
//!
//! Layering follows pi (https://github.com/earendil-works/pi):
//! - [`ai`]: message types, the streaming event protocol, providers (pi: packages/ai)
//! - [`agent`]: the agent loop, tool execution, and tool policy (pi: packages/agent)
//! - [`session`]: append-only JSONL session tree and its projection (pi: SessionManager)
//! - [`runner`]: retries, limits, and persistence around the loop (pi: AgentSession, subset)
//! - [`tools`]: built-in tools

pub mod agent;
pub mod ai;
pub mod prompt;
pub mod runner;
pub mod session;
mod sync;
pub mod tools;
