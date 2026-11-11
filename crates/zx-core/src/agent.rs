//! Agent layer: the loop, tools, policy, and events (pi: packages/agent).
//!
//! - `agent_loop`: the loop, which a run runs once per attempt, what it runs with
//!   ([`LoopConfig`]), and what it needs from the session it records in
//! - `accounting`: what a run spends across its attempts, and its budgets; the loop of each
//!   attempt counts into it
//! - `tool`: the tool interface, and the set of tools a run offers
//! - `hooks`: the extension points around the loop, and the chain that runs them
//! - `policy`: the decision on a tool call, and the policy interface
//! - `interaction`: how a host approves a call the policy asks about, and the refusal when
//!   nobody can
//! - `queue`: messages injected into a running loop, and records extensions ask it to keep
//! - `command`: the slash commands extensions register
//! - `events`: what a run emits, the envelope of each event, and the sink that receives them
//! - `trace`: one run's identity and event numbering, and where its events go: the sink, then
//!   the hooks that observe them
//! - `report`: what a run produced

mod accounting;
mod agent_loop;
mod command;
mod events;
mod hooks;
mod interaction;
mod policy;
mod queue;
mod report;
mod tool;
mod trace;

pub(crate) use accounting::RunAccounting;
pub use agent_loop::LoopConfig;
pub(crate) use agent_loop::{AgentLoop, Halt, PERSIST_FAILED, SessionHost, record_pending_entries};
pub use command::{CommandContext, CommandError, CommandOutcome, SlashCommand, check_command_name};
pub use events::{
    AgentEvent, ApprovalResult, CustomMessageInfo, EVENT_SCHEMA_VERSION, Event, EventSink, Interruption, NullSink,
    RequestKind, ToolVerdict,
};
pub use hooks::{
    CancelSignal, ContextEdit, DEFAULT_HOOK_TIMEOUT, FinishedTurn, HookChain, HookContext, HookError, HookFailure,
    HookOptions, HookPoint, HookResult, HookStage, Hooks, RequestPlan, RequestSettings, TurnDecision, TurnDisposition,
};
pub(crate) use interaction::ApprovalCache;
pub use interaction::{Approval, ApprovalRequest, DEFAULT_APPROVAL_TIMEOUT, Interaction, NoHuman};
pub use policy::{PolicyDecision, ToolPolicy};
pub use queue::{MessageQueues, QueueMode, QueuedMessage};
pub use report::{RunOutcome, RunReport};
pub use tool::{
    ActiveTools, Effects, ExecutionMode, MAX_DETAILS_BYTES, MAX_PROGRESS_BYTES, MAX_QUEUED_PROGRESS, Tool, ToolContext,
    ToolOutput,
};
pub(crate) use trace::EventTarget;
