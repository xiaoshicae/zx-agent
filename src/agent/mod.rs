//! Agent layer: the loop, tools, policy, and events (pi: packages/agent).

pub mod agent_loop;
pub mod events;
pub mod policy;
pub mod queue;
pub mod tool;

pub use agent_loop::{LoopConfig, LoopResult, run_loop};
pub use events::{AgentEvent, EventSink, LoopOutcome, NullSink};
pub use policy::{AllowAll, CommandPolicy, Decision, ToolPolicy};
pub use queue::MessageQueues;
pub use tool::{ExecutionMode, Tool, ToolContext, ToolOutput};
