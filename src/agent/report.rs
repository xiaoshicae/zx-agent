//! What a run produced. Defined in the agent layer so that the `run_end` event can carry it;
//! the runner re-exports both types.

use serde::Serialize;

use crate::ai::Usage;

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    /// The model gave a final answer.
    Completed,
    /// The final answer stayed cut off at the output token limit after the allowed
    /// continuations. `final_text` holds what was produced.
    Truncated,
    /// A request exhausted retries, an operational hook failed, or persistence failed.
    Error,
    /// The turn budget ran out while the model still wanted to continue.
    MaxTurns,
    /// The wall-clock limit was reached.
    Timeout,
    /// The caller cancelled the run.
    Aborted,
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
    pub turns: u32,
    pub retries: u32,
    /// Usage of every response in this run, failed attempts included. A cancelled request
    /// counts as zero: servers report usage only at the end of the stream.
    pub usage: Usage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
}
