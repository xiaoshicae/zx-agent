//! zx-agent: a minimal, general-purpose agent kernel for interactive, unattended, and
//! embedded use. Scenario choices such as prompts and limits belong outside the kernel; the
//! plan is in docs/ROADMAP.md.
//!
//! Layering follows pi (<https://github.com/earendil-works/pi>). The kernel, `zx-core`, is
//! re-exported at the root, one module per domain:
//! - [`Session`], in [`session`]: shared prompts, cancellation, subscriptions and
//!   model switching
//! - [`runner`]: exclusive run ownership, retries, limits and persistence
//! - [`tools`]: the tool interface and the set of tools a run offers
//! - [`policy`]: the interface that decides whether a tool call may run, and approval
//! - [`hooks`]: the extension points around the loop
//! - [`extensions`]: bundles of tools, policies, hooks and prompt sections
//! - [`transcript`]: the append-only record tree of a session, its projection and where it
//!   is stored (pi: SessionManager)
//! - [`compaction`] and [`prompt`]: the bounded summary of a conversation, and system prompt
//!   assembly
//! - [`input`] and [`events`]: what a run is given, and what it emits
//!
//! Beside the kernel:
//! - [`ai`]: message types, the streaming event protocol and the provider contract of
//!   `zx-ai` (pi: packages/ai), with the provider of `zx-ai-openai` as `ai::openai`
//! - [`builtin`]: the implementations of `zx-tools`: the built-in tools (`builtin::tools`),
//!   where they run commands (`builtin::executor`), the policies (`builtin::policy`), the
//!   hooks that continue a cut-off answer, redact known secrets and save long results
//!   (`builtin::continuation`, `builtin::redact`, `builtin::output_store`), and the tools of
//!   external MCP servers (`builtin::mcp`)
//! - [`im`]: chat platforms of `zx-im`, and a server that answers each thread with its own
//!   session, with the platform crates as `im::slack` and `im::feishu`
//! - [`config`]: `config.toml`, the environment: the model, its provider and where commands
//!   run; and the agents, as files: instructions, tools, permissions and limits
//! - [`otel`]: OpenTelemetry traces of runs, exported when an OTLP endpoint is configured
//!
//! An embedder that needs only part of it can depend on those crates directly.

/// The model layer: the types and the provider contract of `zx-ai`, and the OpenAI-compatible
/// provider of `zx-ai-openai` as `openai`.
pub mod ai {
    pub use zx_ai::*;
    pub use zx_ai_openai as openai;
}
/// The lock helper of every crate; not part of the API.
#[doc(hidden)]
pub use zx_ai::sync;
#[doc(hidden)]
pub use zx_ai::text;
pub use zx_core::*;
/// The implementations that ship with zx-agent: the `zx-tools` crate.
pub use zx_tools as builtin;

/// Chat platforms: the interface and the server of `zx-im`, and this workspace's platforms.
pub mod im {
    pub use zx_im::*;
    pub use zx_im_feishu as feishu;
    pub use zx_im_slack as slack;
}

pub mod config;
pub mod otel;
