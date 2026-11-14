//! zx-agent: a minimal, general-purpose agent kernel for interactive, unattended, and
//! embedded use, as the library a program imports. Scenario choices such as prompts and
//! limits belong outside the kernel; the plan is in docs/ROADMAP.md.
//!
//! Layering follows pi (<https://github.com/earendil-works/pi>). This crate has no code of
//! its own: it gives the kernel and what ships with it one name.
//!
//! - The kernel, `zx-core`, at the root, one module per domain. An [`Agent`] is made from an
//!   [`AgentConfig`] and creates or opens [`Session`]s; each prompt returns a [`Run`], which
//!   its caller reads. The interfaces a host may implement are in their modules:
//!   - [`tools`]: the tool interface
//!   - [`policy`]: the interface that decides whether a tool call may run, and approval
//!   - [`hooks`]: the extension points around the loop
//!   - [`extensions`]: bundles of tools, policies, hooks and prompt sections
//!   - [`transcript`]: the append-only record tree of a session, its projection and where
//!     sessions are kept (pi: SessionManager)
//!   - [`compaction`]: the bounded summary of a conversation, and who makes it
//!   - [`agent`]: what a host sets up for one session
//!
//!   and what a run takes and gives is in [`input`], [`events`], [`session`] and [`runner`];
//!   [`prompt`] assembles the system prompt.
//! - [`ai`]: message types, the streaming event protocol and the provider contract of
//!   `zx-ai` (pi: packages/ai), with the provider of `zx-ai-openai` as `ai::openai`
//! - [`builtin`]: the implementations of `zx-tools`: the built-in tools (`builtin::tools`),
//!   where they run commands (`builtin::executor`), the policies (`builtin::policy`), the
//!   hooks that continue a cut-off answer, redact known secrets and save long results
//!   (`builtin::continuation`, `builtin::redact`, `builtin::output_store`), and the tools of
//!   external MCP servers (`builtin::mcp`)
//!
//! What brings an agent to people is built on this library and is not part of it: the
//! command line (`zx-cli`, the program `zx-agent`) and the chat platforms (`zx-im` and its
//! platform crates). An embedder that needs only part of the library can depend on
//! `zx-core`, `zx-ai`, `zx-ai-openai` or `zx-tools` directly.

/// The model layer: the types and the provider contract of `zx-ai`, and the OpenAI-compatible
/// provider of `zx-ai-openai` as `openai`.
pub mod ai {
    pub use zx_ai::*;
    pub use zx_ai_openai as openai;
}
pub use zx_core::*;
/// The implementations that ship with zx-agent: the `zx-tools` crate.
pub use zx_tools as builtin;
