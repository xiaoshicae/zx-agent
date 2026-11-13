//! Kernel of zx-agent: the step loop, sessions and the runner, and the interfaces the kernel
//! defines for everything scenario-specific (tools, policies, hooks, interaction, session
//! storage). The `zx-agent` crate re-exports it whole, module by module.
//!
//! One module per domain, from the bottom layer up. A module uses only modules listed before
//! it, named in brackets where it uses any:
//!
//! - [`transcript`]: the append-only record tree of a session, every record's format and
//!   rules, and the projection the model sees (pi: SessionManager)
//! - [`events`]: what a run emits, and the envelope of each event
//! - `guard`: a run's cancellation as host code sees it ([`CancelSignal`]), and the bound on
//!   every call into host code
//! - [`tools`]: the tool interface, and the set of tools a run offers
//! - [`prompt`]: the system prompt assembled from its sections
//! - [`input`] (transcript): the messages given to a run and the queues that hold them while
//!   it runs
//! - [`policy`] (guard): the interface that decides whether a tool call may run, and how a
//!   host approves a call a policy asks about
//! - [`compaction`] (transcript, events): running one bounded, auditable summary of the
//!   conversation
//! - [`hooks`] (transcript, events, guard, tools, input): the extension points around the
//!   loop, and the chain that runs them
//! - [`extensions`] (tools, prompt, input, policy, hooks): tools, policies, hooks, prompt
//!   sections and slash commands registered together
//! - [`runner`] (all of the above): one run at a time: the step loop of each attempt, the
//!   retries, limits, summaries and recovery around it, and the delivery of its events (pi:
//!   packages/agent)
//! - [`agent_session`] (all of the above): a conversation shared by frontends: prompts,
//!   cancellation, subscriptions and model switching
//!
//! One reference points upward: [`extensions::register`] fills a [`runner::LoopConfig`].
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
//! - **Tool policies and approval**
//!   - records: none; verdicts and approvals are not recorded
//!   - acts: `policy.rs` (the policy interface and its verdict), `policy/chain.rs` (asking
//!     every policy, each bounded), `policy/interaction.rs` (the host interface and the
//!     session's grants), `runner/step_loop/tools.rs` (checking a call and asking the host)
//! - **Events**
//!   - records: none
//!   - acts: `events.rs` (the events and their envelope), `runner/trace.rs` (numbering a
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
//! one owner runs every prompt. The extension interfaces are [`tools::Tool`],
//! [`policy::ToolPolicy`], [`policy::Interaction`], [`hooks::Hook`],
//! [`extensions::Extension`], [`events::EventSink`] and [`transcript::SessionLog`]. The
//! built-in tools, executors, policies and hooks live in the `zx-tools` crate, providers in
//! crates such as `zx-ai-openai`.

// The modules name the model layer and the shared helpers by these crate-root paths.
use zx_ai::{self as ai, sync};

pub mod agent_session;
pub mod compaction;
pub mod events;
pub mod extensions;
mod guard;
pub mod hooks;
pub mod input;
pub mod policy;
pub mod prompt;
pub mod runner;
pub mod tools;
pub mod transcript;

pub use agent_session::AgentSession;
pub use guard::CancelSignal;
