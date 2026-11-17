//! What a run produced, and how a run or one attempt of it ended. Defined beside the events
//! because the `attempt_end` and `run_end` events carry them; `events` re-exports them.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::ai::Usage;

/// How a run, or one attempt of it, ended. In JSON it is the string `outcome`; a budget that
/// ran out adds `budget` and `truncated` beside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RunOutcome {
    /// The model answered without further tool calls and no queued message remained.
    Completed,
    /// The answer stayed cut off at the output token limit and no hook asked to continue it.
    /// A run's `final_text` holds what was produced.
    Truncated,
    /// A provider failed past its retries, an operational hook failed, or the session could
    /// not record a step.
    Failed,
    /// `budget` left no room for the request the model still wanted, such as one that
    /// continues an answer cut off at the output token limit. `truncated` says whether the
    /// run's last answer, in its `final_text`, was cut off so.
    BudgetExhausted { budget: Budget, truncated: bool },
    /// The run's wall-clock limit was reached. Only a run ends this way: its attempt reports
    /// `Cancelled`, the cancellation the timer caused.
    TimedOut,
    /// Cancelled by the caller.
    Cancelled,
}

/// A budget of a run, which ends the run once it leaves no room for another request. In JSON
/// each is named after the limit that sets it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Budget {
    /// The model requests a run may make, summary requests included.
    #[serde(rename = "model_request_limit")]
    ModelRequests,
    /// The input and output tokens a run may use.
    #[serde(rename = "token_budget")]
    Tokens,
    /// What a run may cost.
    #[serde(rename = "cost_budget")]
    Cost,
}

impl RunOutcome {
    /// The outcome as JSON names it, such as `completed` or `budget_exhausted`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Truncated => "truncated",
            Self::Failed => "failed",
            Self::BudgetExhausted { .. } => "budget_exhausted",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
        }
    }
}

impl Budget {
    /// The budget as JSON names it, after the limit that sets it, such as `token_budget`.
    pub fn name(self) -> &'static str {
        match self {
            Self::ModelRequests => "model_request_limit",
            Self::Tokens => "token_budget",
            Self::Cost => "cost_budget",
        }
    }
}

/// What a finished run reports.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunReport {
    #[serde(flatten)]
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
    #[serde(skip_serializing_if = "zx_util::num::is_zero")]
    pub denied_calls: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The session the run belongs to.
    pub session_id: String,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn names_are_the_ones_json_gives() {
        let budgets = [Budget::ModelRequests, Budget::Tokens, Budget::Cost];
        for budget in budgets {
            assert_eq!(serde_json::to_value(budget).unwrap(), json!(budget.name()));
        }
        let outcomes = [
            RunOutcome::Completed,
            RunOutcome::Truncated,
            RunOutcome::Failed,
            RunOutcome::BudgetExhausted {
                budget: Budget::Tokens,
                truncated: true,
            },
            RunOutcome::TimedOut,
            RunOutcome::Cancelled,
        ];
        for outcome in outcomes {
            assert_eq!(serde_json::to_value(outcome).unwrap()["outcome"], json!(outcome.name()));
        }
    }

    #[test]
    fn a_budget_that_ran_out_says_which_and_whether_the_answer_was_cut_off() {
        let outcome = RunOutcome::BudgetExhausted {
            budget: Budget::ModelRequests,
            truncated: true,
        };
        assert_eq!(
            serde_json::to_value(outcome).unwrap(),
            json!({"outcome": "budget_exhausted", "budget": "model_request_limit", "truncated": true})
        );
        assert_eq!(
            serde_json::to_value(RunOutcome::Completed).unwrap(),
            json!({"outcome": "completed"})
        );
    }
}
