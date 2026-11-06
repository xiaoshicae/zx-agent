//! Agent layer: the loop, tools, policy, and events (pi: packages/agent).

mod agent_loop;
mod command;
mod events;
mod hooks;
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
pub use policy::{AllowAll, Decision, ToolPolicy};
pub use queue::{MessageQueues, PendingEntry, QueueMode, QueuedMessage};
pub use report::{RunOutcome, RunReport};
pub(crate) use session_host::{ContextPreparation, Prepared, SessionHost, record_pending_entries};
pub use tool::{
    ActiveTools, Effects, ExecutionMode, MAX_DETAILS_BYTES, MAX_PROGRESS_BYTES, MAX_QUEUED_PROGRESS, Tool, ToolContext,
    ToolOutput,
};
