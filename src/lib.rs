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
//! - [`profile`]: scenarios as files: instructions, tools, rules and limits
//! - [`prompt`]: system prompt assembly and the generated environment section
//! - [`tools`]: built-in tools
//! - [`executor`]: where built-in tools run commands and read or write files
//! - [`output_store`]: long tool results saved to files, paged with `read_output`
//! - [`continuation`] and [`redact`]: the built-in hooks that continue a cut-off answer and
//!   redact known secrets
//! - [`im`]: chat platforms, and a server that answers each thread with its own session
//! - [`extensions`]: bundles of tools, hooks and prompt sections
//! - [`mcp`]: the tools of external MCP servers, as an extension
//! - [`otel`]: OpenTelemetry traces and metrics of runs, exported when an OTLP endpoint is
//!   configured
//!
//! The modules live in the crates of this workspace and are re-exported here under the paths
//! they always had: [`ai`] is `zx-ai`, with the provider of `zx-ai-openai` as `ai::openai`;
//! the kernel ([`agent`], [`session`], [`runner`] and the rest) is `zx-core`; [`tools`],
//! [`executor`], [`output_store`], [`policy`], [`continuation`], [`redact`] and [`mcp`] are
//! `zx-tools`; [`im`] is `zx-im`, with the platform crates `zx-im-slack` and `zx-im-feishu`
//! as `im::slack` and `im::feishu`. An embedder that needs only part of it can depend on
//! those crates directly.

/// The model layer: the types and the provider contract of `zx-ai`, and the OpenAI-compatible
/// provider of `zx-ai-openai` as `openai`, with its SSE parser as `sse`.
pub mod ai {
    pub use zx_ai::*;
    pub use zx_ai_openai as openai;
    pub use zx_ai_openai::sse;
}
/// The lock helper of every crate; not part of the API.
#[doc(hidden)]
pub use zx_ai::sync;
#[doc(hidden)]
pub use zx_ai::text;
pub use zx_core::{AgentSession, agent, agent_session, compaction, extensions, interaction, prompt, runner, session};
#[doc(hidden)]
pub use zx_tools::files;
pub use zx_tools::{continuation, executor, mcp, output_store, policy, redact, tools};

/// Chat platforms: the interface and the server of `zx-im`, and this workspace's platforms.
pub mod im {
    pub use zx_im::*;
    pub use zx_im_feishu as feishu;
    pub use zx_im_slack as slack;
}

pub mod otel;
pub mod profile;
