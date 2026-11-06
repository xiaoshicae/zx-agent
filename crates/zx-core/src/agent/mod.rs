//! Agent layer: the loop, tools, policy, and events (pi: packages/agent).

pub mod agent_loop;
pub mod command;
pub mod events;
pub mod hooks;
pub mod policy;
pub mod queue;
mod report;
mod session_host;
pub mod tool;

pub use agent_loop::{LoopConfig, LoopControl, LoopFailure, LoopResult, LoopSpec, run_loop};
pub(crate) use agent_loop::{price, record_pending_entries};
pub use command::{CommandContext, CommandError, CommandOutcome, SlashCommand, check_command_name};
pub use events::{
    AgentEvent, ApprovalResult, CustomMessageInfo, EVENT_SCHEMA_VERSION, Event, EventSink, Interruption, LoopOutcome,
    NullSink, RequestKind, ToolVerdict,
};
pub(crate) use events::{EventTarget, RunTrace};
pub use hooks::{
    CancelSignal, ContextEdit, HookChain, HookContext, HookError, HookFailure, HookOptions, HookPhase, HookResult,
    HookStage, Hooks, RequestPlan, RequestSettings, TurnDecision, TurnDisposition, TurnSummary,
};
pub use policy::{AllowAll, Decision, ToolPolicy};
pub use queue::{MessageQueues, PendingEntry, QueueMode, QueuedMessage};
pub use report::{RunOutcome, RunReport};
pub(crate) use session_host::{ContextPreparation, Prepared, SessionHost};
pub use tool::{
    ActiveTools, Effects, ExecutionMode, MAX_DETAILS_BYTES, MAX_PROGRESS_BYTES, MAX_QUEUED_PROGRESS, Tool, ToolContext,
    ToolOutput,
};
