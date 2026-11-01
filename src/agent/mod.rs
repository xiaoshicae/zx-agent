//! Agent layer: the loop, tools, policy, and events (pi: packages/agent).

pub mod agent_loop;
pub mod events;
pub mod hooks;
mod host;
pub mod policy;
pub mod queue;
mod report;
pub mod tool;

pub use agent_loop::{LoopConfig, LoopControl, LoopFailure, LoopResult, LoopSpec, run_loop};
pub use events::{
    AgentEvent, ApprovalResult, EVENT_SCHEMA_VERSION, Event, EventSink, Interruption, LoopOutcome, NullSink,
    RequestKind, ToolVerdict,
};
pub(crate) use events::{Emit, RunTrace};
pub use hooks::{
    ContextEdit, HookChain, HookContext, HookError, HookFailure, HookOptions, HookPhase, HookResult, HookStage, Hooks,
    TurnDecision, TurnDisposition, TurnSummary,
};
pub(crate) use host::{ContextPreparation, SessionHost};
pub use policy::{AllowAll, Decision, ToolPolicy};
pub use queue::MessageQueues;
pub use report::{RunOutcome, RunReport};
pub use tool::{Effects, ExecutionMode, Tool, ToolContext, ToolOutput};
