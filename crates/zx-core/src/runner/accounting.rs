//! Aggregate attempt costs and calculate the remaining run budget.

use std::collections::BTreeMap;

use super::Limits;
use crate::agent::AttemptResult;
use crate::ai::Usage;

#[derive(Default)]
pub(super) struct RunAccounting {
    /// Attempts made: the first, retries and recoveries from context overflow.
    pub attempts: u32,
    pub task_turns: u32,
    pub model_requests: u32,
    pub retries: u32,
    pub answer_continuations: u32,
    pub usage: Usage,
    /// What the tools spent on work of their own, by tool name.
    pub tool_usage: BTreeMap<String, Usage>,
    /// Tool calls that a policy or an approval denied.
    pub denied_calls: u32,
    /// A context overflow was recovered from once already.
    pub overflow_recovered: bool,
}

impl RunAccounting {
    pub(super) fn record(&mut self, attempt: &AttemptResult) {
        self.attempts = self.attempts.saturating_add(1);
        self.task_turns = self.task_turns.saturating_add(attempt.task_turns);
        self.model_requests = self.model_requests.saturating_add(attempt.model_requests);
        self.answer_continuations = attempt.answer_continuations;
        self.usage.add(&attempt.usage);
        self.denied_calls = self.denied_calls.saturating_add(attempt.denied_calls);
        for (tool, usage) in &attempt.tool_usage {
            self.tool_usage.entry(tool.clone()).or_default().add(usage);
        }
    }

    pub(super) fn remaining_model_requests(&self, limits: &Limits) -> Option<u32> {
        limits
            .max_model_requests
            .map(|limit| limit.saturating_sub(self.model_requests))
    }

    pub(super) fn remaining_tokens(&self, limits: &Limits) -> Option<u64> {
        limits
            .token_budget
            .map(|limit| limit.saturating_sub(self.usage.input_and_output()))
    }

    /// What is left of the cost budget, by the prices of the responses so far; never negative.
    pub(super) fn remaining_cost(&self, limits: &Limits) -> Option<f64> {
        limits
            .cost_budget
            .map(|limit| (limit - self.usage.total_cost.unwrap_or(0.0)).max(0.0))
    }

    pub(super) fn has_budget(&self, limits: &Limits) -> bool {
        self.remaining_model_requests(limits) != Some(0)
            && self.remaining_tokens(limits) != Some(0)
            && self.remaining_cost(limits).is_none_or(|left| left > 0.0)
    }
}
