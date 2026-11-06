//! Agent layer: the loop, tools, policy, and events (pi: packages/agent).
//!
//! - `agent_loop`: one attempt of the loop (`run_loop`) and what it runs with
//! - `tool`: the tool interface, and the set of tools a run offers
//! - `hooks`: the extension points around the loop, and the chain that runs them
//! - `policy`: the decision on a tool call, and the policy interface
//! - `interaction`: how a host approves a call the policy asks about, and the refusal when
//!   nobody can
//! - `queue`: messages injected into a running loop, and records extensions ask it to keep
//! - `command`: the slash commands extensions register
//! - `events`: what a run emits, the envelope of each event, and the sink that receives them
//! - `report`: what a run produced
//! - `session_host`: what the loop needs from the session it records in

mod agent_loop;
mod command;
mod events;
mod hooks;
mod interaction;
mod policy;
mod queue;
mod report;
mod session_host;
mod tool;

pub use agent_loop::{LoopConfig, LoopControl, LoopFailure, LoopResult, LoopSpec, run_loop};
pub use command::{CommandContext, CommandError, CommandOutcome, SlashCommand, check_command_name};
pub use events::{
    AgentEvent, ApprovalResult, CustomMessageInfo, EVENT_SCHEMA_VERSION, Event, EventSink, Interruption, LoopOutcome,
    NullSink, RequestKind, ToolVerdict,
};
pub(crate) use events::{EventTarget, RunTrace};
pub use hooks::{
    CancelSignal, ContextEdit, DEFAULT_HOOK_TIMEOUT, HookChain, HookContext, HookError, HookFailure, HookOptions,
    HookPhase, HookResult, HookStage, Hooks, RequestPlan, RequestSettings, TurnDecision, TurnDisposition, TurnSummary,
};
pub use interaction::{Approval, ApprovalCache, ApprovalRequest, DEFAULT_APPROVAL_TIMEOUT, Interaction, NoHuman};
pub use policy::{AllowAll, Decision, ToolPolicy};
pub use queue::{MessageQueues, PendingEntry, QueueMode, QueuedMessage};
pub use report::{RunOutcome, RunReport};
pub(crate) use session_host::{ContextPreparation, Prepared, SessionHost, record_pending_entries};
pub use tool::{
    ActiveTools, Effects, ExecutionMode, MAX_DETAILS_BYTES, MAX_PROGRESS_BYTES, MAX_QUEUED_PROGRESS, Tool, ToolContext,
    ToolOutput,
};
