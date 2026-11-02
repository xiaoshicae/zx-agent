//! Kernel of zx-agent: the agent loop, sessions and the runner, and the seams the kernel
//! defines for everything scenario-specific (tools, hooks, policies, interaction, executors,
//! session storage). The `zx-agent` crate re-exports these modules under the same names.
//!
//! - [`agent`]: the agent loop, tool execution, and tool policy (pi: packages/agent)
//! - [`session`]: append-only JSONL session tree and its projection (pi: SessionManager)
//! - [`agent_session`]: shared prompts, cancellation, subscriptions and model switching
//! - [`runner`]: exclusive run ownership, retries, limits and persistence
//! - [`prompt`]: system prompt assembly and the generated environment section
//! - [`executor`]: where built-in tools run commands and read files
//! - [`extensions`]: bundles of tools, hooks and prompt sections, and the built-in policies

// The modules name the model layer and the shared helpers by these crate-root paths.
use zx_ai::{self as ai, sync, text};

pub mod agent;
pub mod agent_session;
pub mod compaction;
pub mod executor;
pub mod extensions;
pub mod interaction;
pub mod output_store;
pub mod prompt;
pub mod runner;
pub mod session;

pub use agent_session::AgentSession;
