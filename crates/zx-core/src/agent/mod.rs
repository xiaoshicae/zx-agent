//! Agent layer: the loop, tools, policy, and events (pi: packages/agent).

pub mod agent_loop;
pub mod events;
pub mod hooks;
pub mod policy;
pub mod queue;
mod report;
mod session_runtime;
pub mod tool;

pub use agent_loop::{LoopConfig, LoopControl, LoopFailure, LoopResult, LoopSpec, run_loop};
pub use events::{
    AgentEvent, ApprovalResult, EVENT_SCHEMA_VERSION, Event, EventSink, Interruption, LoopOutcome, NullSink,
    RequestKind, ToolVerdict,
};
pub(crate) use events::{Emit, RunTrace};
pub use hooks::{
    CancelSignal, ContextEdit, HookChain, HookContext, HookError, HookFailure, HookOptions, HookPhase, HookResult,
    HookStage, Hooks, RequestPlan, RequestSettings, TurnDecision, TurnDisposition, TurnSummary,
};
pub use policy::{AllowAll, Decision, ToolPolicy};
pub use queue::MessageQueues;
pub use report::{RunOutcome, RunReport};
pub(crate) use session_runtime::{ContextPreparation, SessionRuntime};
pub use tool::{Effects, ExecutionMode, MAX_DETAILS_BYTES, MAX_PROGRESS_BYTES, Tool, ToolContext, ToolOutput};
