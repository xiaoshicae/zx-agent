//! What a run produced, and how a run or one attempt of it ended. Defined in the agent layer
//! so that the `agent_end` and `run_end` events can carry them; the runner re-exports both.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::ai::Usage;

/// How a run, or one attempt of it, ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    /// The model answered without further tool calls and no queued message remained.
    Completed,
    /// The answer stayed cut off at the output token limit and no hook asked to continue it.
    /// A run's `final_text` holds what was produced.
    Truncated,
    /// A provider failed past its retries, an operational hook failed, or the session could
    /// not record a step.
    Error,
    /// The model request budget ran out while the model still wanted to continue.
    ModelRequestLimit,
    /// The run's wall-clock limit was reached. Only a run ends this way: its attempt reports
    /// `Aborted`, the cancellation the timer caused.
    Timeout,
    /// Cancelled.
    Aborted,
    /// The token budget ran out while the model still wanted to continue.
    TokenBudget,
    /// The cost budget ran out while the model still wanted to continue.
    CostBudget,
}

/// Summary of a finished run.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunReport {
    pub outcome: RunOutcome,
    /// Text of the last assistant message in the session projection, joined with the parts
    /// it continues when earlier responses were cut off at the output token limit.
    pub final_text: String,
    /// Model requests made, summary requests included, saturating at `u32::MAX` for
    /// unbounded runs.
    pub model_requests: u32,
    pub retries: u32,
    /// Usage of every response in this run, failed attempts included. A cancelled request
    /// counts as zero: servers report usage only at the end of the stream.
    pub usage: Usage,
    /// What the tools spent on work of their own, by tool name (pi: the usage of tool
    /// results); only the tools that reported some. Not part of `usage`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub tool_usage: BTreeMap<String, Usage>,
    /// Tool calls that a policy or an approval denied, so that they did not run. The model
    /// sees each as an error result and may still complete the task, so a host that must
    /// know whether every intended action ran checks this.
    #[serde(skip_serializing_if = "is_zero")]
    pub denied_calls: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
}

fn is_zero(count: &u32) -> bool {
    *count == 0
}
