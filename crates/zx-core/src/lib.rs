//! Kernel of zx-agent: the step loop, sessions and the runner, and the interfaces the kernel
//! defines for everything scenario-specific (tools, hooks, policies, interaction, session
//! storage). The `zx-agent` crate re-exports the public items under the same names.
//!
//! The modules, from the bottom layer up; each uses only the modules listed before it:
//!
//! - [`transcript`]: the append-only record tree of a session, every record's format and
//!   rules, and the projection the model sees (pi: SessionManager)
//! - `events`: what a run emits, the envelope of each event, and where a run's events go
//! - `input`: the messages given to a run and the queues that hold them while it runs
//! - `tools`: the tool interface, and the set of tools a run offers
//! - `policy`: the verdict on a tool call, and how a host approves a call the policy asks
//!   about
//! - `hooks`: the extension points around the loop, and the chain that runs them
//! - [`extensions`]: tools, hooks, prompt sections and slash commands registered together
//! - [`prompt`]: the system prompt assembled from its sections
//! - [`compaction`]: running one bounded, auditable summary of the conversation
//! - [`runner`]: one run at a time: the step loop of each attempt, and the retries, limits,
//!   summaries and recovery around it (pi: packages/agent)
//! - [`agent_session`]: a conversation shared by frontends: prompts, cancellation,
//!   subscriptions and model switching
//!
//! # Where each feature lives
//!
//! The transcript owns what a feature records: the format, the checks a record passes both
//! when it is appended and when a session is loaded, and the queries the projection and
//! recovery need. The layers above act on it. A feature therefore spans several modules. For
//! each feature below, "records" names where the transcript keeps its records and rules, and
//! "acts" names the code above it that acts on them:
//!
//! - **Compaction** (the summary is the text it produces)
//!   - records: `transcript/compaction.rs` (types and limits),
//!     `transcript/compaction/records.rs` (appending and checking the records),
//!     `transcript/compaction/source.rs` (what a compaction reads),
//!     `transcript/projection.rs` (the summary in the context, and which answer's usage
//!     still measures it)
//!   - acts: `compaction.rs` (budgets, the token estimate and report),
//!     `compaction/compactor.rs` (one compaction), `compaction/answer.rs` (the model's
//!     answer), `runner/run_host.rs` (when to compact)
//! - **Interrupted tool calls**
//!   - records: `transcript/recovery.rs` (start records, the calls left without results)
//!   - acts: `runner/step_loop/tools.rs` (records the start of a call with side effects),
//!     `runner/interrupted.rs` (records their results)
//! - **Request audit**
//!   - records: `transcript/request.rs` (request settings and per-request checkpoints)
//!   - acts: `runner/step_loop/model.rs` (task requests), `compaction/compactor.rs` (summary
//!     requests)
//! - **Context edits**
//!   - records: `transcript.rs` (the rules of an edit), `transcript/projection.rs` (applying
//!     edits)
//!   - acts: `hooks.rs` (checking a hook's plan of edits before any is recorded),
//!     `runner/step_loop/context.rs` (input and hook edits), `runner/run_host.rs` (omitting
//!     a failed answer)
//! - **Session names**
//!   - records: `transcript/name.rs` (the `session_info` record, the latest name),
//!     `transcript/directory.rs` (the name a listing shows)
//!   - acts: `agent_session.rs` (naming a session while idle)
//! - **Approval**
//!   - records: none; approvals are not recorded
//!   - acts: `policy/interaction.rs` (the host interface and the session's grants),
//!     `runner/step_loop/tools.rs` (asking)
//! - **Events**
//!   - records: none
//!   - acts: `events.rs` (the events and their envelope), `events/trace.rs` (numbering a
//!     run's events and delivering them to the sink and the observing hooks),
//!     `agent_session/subscribers.rs` (fan-out to subscribers)
//!
//! A session holds a conversation across runs. Each [`runner::Runner::run`] processes one
//! prompt and may retry a failed provider response as another loop attempt. A task step is
//! one task response and its tool calls; a model request may also be a compaction summary,
//! which consumes the request budget without creating a separate task step. Retries and
//! summary requests both consume the run's model-request budget.
//!
//! Embedders start with [`AgentSession`], or with [`runner::Runner`], its exclusive half, when
//! one owner runs every prompt. [`transcript::SessionLog`], [`Tool`], [`Hook`] and
//! [`Interaction`] are extension interfaces. The built-in tools, executors, policies and hooks
//! live in the `zx-tools` crate, providers in crates such as `zx-ai-openai`.

// The modules name the model layer and the shared helpers by these crate-root paths.
use zx_ai::{self as ai, sync};

pub mod agent_session;
pub mod compaction;
mod events;
pub mod extensions;
mod hooks;
mod input;
mod policy;
pub mod prompt;
pub mod runner;
mod tools;
pub mod transcript;

pub use agent_session::AgentSession;
pub(crate) use events::EventTarget;
pub use events::{
    ApprovalResult, CustomMessageInfo, EVENT_SCHEMA_VERSION, Event, EventKind, EventSink, HookPoint, Interruption,
    NullSink, RequestKind, ToolVerdict,
};
pub use extensions::{CommandContext, CommandError, CommandOutcome, SlashCommand, check_command_name};
pub use hooks::{
    CancelSignal, ContextEdit, DEFAULT_HOOK_TIMEOUT, FinishedStep, Hook, HookChain, HookContext, HookError,
    HookFailure, HookOptions, HookResult, HookStage, RequestPlan, RequestSettings, StepDecision, StepDisposition,
};
pub use input::{Input, InputQueue, QueueMode};
pub(crate) use policy::ApprovalCache;
pub use policy::{
    Approval, ApprovalRequest, DEFAULT_APPROVAL_TIMEOUT, Interaction, NoHuman, PolicyChain, PolicyContext,
    PolicyDecision, ToolPolicy,
};
pub(crate) use runner::{Halt, PERSIST_FAILED, RunAccounting, SessionHost};
pub use runner::{LoopConfig, RunOutcome, RunReport};
pub use tools::{
    ActiveTools, Effects, ExecutionMode, MAX_DETAILS_BYTES, MAX_PROGRESS_BYTES, MAX_QUEUED_PROGRESS, Tool, ToolContext,
    ToolOutput,
};
