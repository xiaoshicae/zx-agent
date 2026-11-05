//! Kernel of zx-agent: the agent loop, sessions and the runner, and the seams the kernel
//! defines for everything scenario-specific (tools, hooks, policies, interaction, executors,
//! session storage). The `zx-agent` crate re-exports these modules under the same names.
//!
//! - [`agent`]: the agent loop, tool execution, and tool policy (pi: packages/agent)
//! - [`session`]: append-only JSONL session tree and its projection (pi: SessionManager)
//! - [`agent_session`]: shared prompts, cancellation, subscriptions and model switching
//! - [`runner`]: exclusive run ownership, retries, limits and persistence
//! - [`compaction`]: bounded, auditable summaries of the conversation
//! - [`prompt`]: system prompt assembly and the generated environment section
//! - [`interaction`]: how a host approves tool calls, and the refusal when nobody can
//! - [`extensions`]: bundles of tools, hooks and prompt sections
//!
//! A session holds a conversation across runs. Each [`runner::Runner::run`] processes one
//! prompt and may retry a failed provider response as another loop attempt. A task turn is
//! one task response and its tool calls; a model request may also be a compaction summary,
//! which consumes the request budget without creating a separate task turn. Retries and
//! summary requests both consume the run's model-request budget.
//!
//! Embedders normally start with [`AgentSession`] or [`runner::Runner`]. [`agent::run_loop`]
//! exposes the lower-level single-attempt loop. [`session::SessionLog`], [`agent::Tool`],
//! [`agent::Hooks`], and [`interaction::Interaction`] are extension interfaces. The built-in
//! tools, executors, policies and hooks live in the `zx-tools` crate, providers in crates
//! such as `zx-ai-openai`.

// The modules name the model layer and the shared helpers by these crate-root paths.
use zx_ai::{self as ai, sync};

pub mod agent;
pub mod agent_session;
pub mod compaction;
pub mod extensions;
pub mod interaction;
pub mod prompt;
pub mod runner;
pub mod session;

pub use agent_session::AgentSession;
