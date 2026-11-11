//! Kernel of zx-agent: the agent loop, sessions and the runner, and the seams the kernel
//! defines for everything scenario-specific (tools, hooks, policies, interaction, executors,
//! session storage). The `zx-agent` crate re-exports these modules under the same names.
//!
//! The modules, from the bottom layer up; each uses only the modules listed before it:
//!
//! - [`session`]: the append-only session tree, every record's format and rules, and the
//!   projection the model sees (pi: SessionManager)
//! - [`agent`]: the loop of one attempt, tools, hooks, policy and approval, and events (pi:
//!   packages/agent)
//! - [`compaction`]: running one bounded, auditable summary of the conversation
//! - [`runner`]: one run at a time: retries, limits, summaries and recovery around the loop
//! - [`agent_session`]: a conversation shared by frontends: prompts, cancellation,
//!   subscriptions and model switching
//!
//! Beside them, [`prompt`] assembles the system prompt and [`extensions`] bundles tools, hooks
//! and prompt sections for a [`agent::LoopConfig`].
//!
//! # Where each feature lives
//!
//! The session owns what a feature records: the format, the checks a record passes both when
//! it is appended and when a session is loaded, and the queries the projection and recovery
//! need. The layers above act on it. A feature therefore spans several modules:
//!
//! | Feature | Records and rules, in `session` | What acts, above it |
//! |---|---|---|
//! | Compaction; the summary is the text it produces | `session/compaction.rs` (types and limits), `compaction/records.rs` (appending and checking the records), `compaction/source.rs` (what a compaction reads), `session/projection.rs` (the summary in the context) | `compaction.rs` (budgets and report), `compaction/compactor.rs` (one compaction), `compaction/answer.rs` (the model's answer), `runner/run_host.rs` (when to compact) |
//! | Interrupted tool calls | `session/recovery.rs` (start records, the calls left without results) | `agent/agent_loop/tools.rs` (records the start of a call with side effects), `runner/interrupted.rs` (records their results) |
//! | Request audit | `session/request.rs` (request settings and per-request checkpoints) | `agent/agent_loop/model.rs` (task requests), `compaction/compactor.rs` (summary requests) |
//! | Context edits | `session.rs` (the rules of an edit), `session/projection.rs` (applying edits) | `agent/agent_loop/context.rs` (input and hook edits), `runner/run_host.rs` (omitting a failed answer) |
//! | Session names | `session/name.rs` (the `session_info` record, the latest name), `session/directory.rs` (the name a listing shows) | `agent_session.rs` (naming a session while idle) |
//! | Approval | none: approvals are not recorded | `agent/interaction.rs` (the host interface and the session's grants), `agent/agent_loop/tools.rs` (asking) |
//! | Events | none | `agent/events.rs` (the events and their envelope), `agent_session/subscribers.rs` (fan-out to subscribers) |
//!
//! A session holds a conversation across runs. Each [`runner::Runner::run`] processes one
//! prompt and may retry a failed provider response as another loop attempt. A task turn is
//! one task response and its tool calls; a model request may also be a compaction summary,
//! which consumes the request budget without creating a separate task turn. Retries and
//! summary requests both consume the run's model-request budget.
//!
//! Embedders start with [`AgentSession`], or with [`runner::Runner`], its exclusive half, when
//! one owner runs every prompt. [`session::SessionLog`], [`agent::Tool`],
//! [`agent::Hooks`], and [`agent::Interaction`] are extension interfaces. The built-in
//! tools, executors, policies and hooks live in the `zx-tools` crate, providers in crates
//! such as `zx-ai-openai`.

// The modules name the model layer and the shared helpers by these crate-root paths.
use zx_ai::{self as ai, sync};

pub mod agent;
pub mod agent_session;
pub mod compaction;
pub mod extensions;
pub mod prompt;
pub mod runner;
pub mod session;

pub use agent_session::AgentSession;
