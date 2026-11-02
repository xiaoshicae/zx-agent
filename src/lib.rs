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
//! - [`executor`]: where built-in tools run commands and read files
//! - [`im`]: chat platforms, and a server that answers each thread with its own session
//! - [`extensions`]: bundles of tools, hooks and prompt sections, and the built-in policies
//! - [`mcp`]: the tools of external MCP servers, as an extension
//! - [`otel`]: OpenTelemetry traces of runs, exported when an OTLP endpoint is configured
//!
//! The modules live in the crates of this workspace and are re-exported here under the paths
//! they always had: [`ai`] is `zx-ai`; the kernel ([`agent`], [`session`], [`runner`] and the
//! rest) is `zx-core`; [`tools`], [`policy`] and [`mcp`] are `zx-tools`; [`im`] is `zx-im`.
//! An embedder that needs only part of it can depend on those crates directly.

pub use zx_ai as ai;
#[doc(hidden)]
pub use zx_ai::text;
pub use zx_core::{
    AgentSession, agent, agent_session, compaction, executor, extensions, interaction, output_store, prompt, runner,
    session,
};
pub use zx_im as im;
pub use zx_tools::{mcp, policy, tools};

pub mod otel;
pub mod presets;
pub mod profile;
