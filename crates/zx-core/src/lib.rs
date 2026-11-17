//! Kernel of zx-agent: the agent and its sessions, the runner and the step loop, and the
//! interfaces the kernel defines for everything scenario-specific (tools, policies, hooks,
//! session storage, session setup). The `zx-agent` crate re-exports it whole, module by
//! module.
//!
//! One module per domain, from the bottom layer up. A module uses only modules listed before
//! it, named in brackets where it uses any:
//!
//! - [`transcript`]: the append-only record tree of a session, every record's format and
//!   rules, the projection the model sees, and where sessions are kept (pi: SessionManager)
//! - [`events`]: what a run emits, and the envelope of each event
//! - `guard`: a run's cancellation as host code sees it ([`CancelSignal`]), and the bound on
//!   every call into host code
//! - [`tools`]: the tool interface, the tools of a session, and which of them a run offers
//! - [`input`] (transcript): the messages given to a run and the queues that hold them while
//!   it runs
//! - [`policy`] (guard): the interface that decides whether a tool call may run, and the
//!   approval of a call a policy asks about
//! - [`compaction`] (transcript, events, guard): running one bounded, auditable summary of
//!   the conversation, which the kernel asks the model for or the host's summarizer makes
//! - [`hooks`] (transcript, events, guard, tools, input): the extension points around the
//!   loop, and the chain that runs them
//! - [`extensions`] (tools, input, policy, hooks): tools, policies, hooks, prompt sections and
//!   slash commands registered together
//! - `runner` (all of the above): one run at a time: the step loop of each attempt, the
//!   retries, limits, summaries and recovery around it, and the delivery of its events (pi:
//!   packages/agent)
//! - [`session`] (all of the above): a conversation shared by frontends, and the run each
//!   prompt, continuation or compaction returns: its events and its result
//! - [`agent`] (all of the above): the role every session plays, the host's setup of what
//!   belongs to one session, and the sessions that are open
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
//!     `transcript/storage.rs` (the name a listing shows)
//!   - acts: `session.rs` (naming a session while idle)
//! - **Tool policies and approval**
//!   - records: none; verdicts and approvals are not recorded
//!   - acts: `policy.rs` (the policy interface and its verdict), `policy/chain.rs` (asking
//!     every policy, each bounded), `policy/approval.rs` (the gate where a call waits for the
//!     host's answer, and the session's grants), `runner/step_loop/tools.rs` (checking a
//!     call and waiting at the gate), `session.rs` (the host's answer)
//! - **Events**
//!   - records: none
//!   - acts: `events.rs` (the events and their envelope), `runner/trace.rs` (numbering a
//!     run's events and delivering them to the run's reader and the observing hooks),
//!     `session/run.rs` (the reader's end: the stream of a run's events)
//!
//! An [`Agent`] creates and opens sessions. A session holds a conversation across runs. Each
//! [`Session::send`] starts a [`Run`], which processes one prompt and may retry a failed
//! provider response as another loop attempt. A task step is one task response and its tool calls; a model request may also
//! be a compaction summary, which consumes the request budget without creating a separate
//! task step. Retries and summary requests both consume the run's model-request budget.
//!
//! Embedders make an [`Agent`] from an [`AgentConfig`], ask it for a [`Session`], and read
//! the [`Run`] each prompt returns. The extension interfaces are [`tools::Tool`],
//! [`policy::ToolPolicy`], [`hooks::Hook`], [`extensions::Extension`],
//! [`agent::SessionSetup`], and [`transcript::SessionStorage`] with its
//! [`transcript::SessionLog`]; approvals are answered through the session. The storage in
//! memory ships with the kernel; the storage of session files, the built-in tools,
//! executors, policies and hooks live in the `zx-tools` crate, providers in crates such as
//! `zx-ai-openai`.

// The modules name the model layer and the shared helpers by these crate-root paths.
use zx_ai as ai;
use zx_util::{sync, text};

pub mod agent;
pub mod compaction;
pub mod events;
pub mod extensions;
mod guard;
pub mod hooks;
pub mod input;
pub mod policy;
mod runner;
pub mod session;
pub mod tools;
pub mod transcript;

pub use agent::{Agent, AgentConfig};
pub use guard::CancelSignal;
pub use session::{Run, Session, SessionOptions};
