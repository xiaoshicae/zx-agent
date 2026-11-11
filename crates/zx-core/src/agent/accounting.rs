//! What a run spends across its attempts, and the budgets it spends against. The run owns
//! the accounting and lends it to the loop of each attempt, which counts every request,
//! response and tool call as it happens and checks the budgets before each request; the
//! run reads the totals for its report.

use std::collections::BTreeMap;

use super::report::RunOutcome;
use crate::ai::{ToolResultMessage, Usage};

/// What a run has spent so far, over all its attempts, and its budgets.
#[derive(Default)]
pub(crate) struct RunAccounting {
    /// Model requests the run may make, summary requests included; `None` is unlimited,
    /// `Some(0)` makes no request.
    max_model_requests: Option<u32>,
    /// Tokens the run may use, input plus output as the provider reports them, summary
    /// requests included; `None` is unlimited.
    token_budget: Option<u64>,
    /// What the run may spend, in the unit of the configured prices; a response without a
    /// price counts as nothing. `None` is unlimited.
    cost_budget: Option<f64>,
    /// Attempts started: the first, retries and recoveries from context overflow. The
    /// attempt in progress is the last one.
    pub(crate) attempts: u32,
    /// Task turns started; the turn in progress is the last one, so turn numbers go on
    /// across attempts.
    pub(crate) task_turns: u32,
    /// Model requests made, summary requests included: what the request budget counts.
    pub(crate) model_requests: u32,
    pub(crate) retries: u32,
    /// Answer continuations scheduled.
    pub(crate) answer_continuations: u32,
    /// Usage of every response, failed ones and summary responses included: what the token
    /// and cost budgets count.
    pub(crate) usage: Usage,
    /// What the tools spent on work of their own, by tool name: the `usage` of their
    /// results. Not part of `usage`, which the token budget counts.
    pub(crate) tool_usage: BTreeMap<String, Usage>,
    /// Tool calls that a policy or an approval denied.
    pub(crate) denied_calls: u32,
}

impl RunAccounting {
    /// Nothing spent yet, against these budgets.
    pub(crate) fn new(max_model_requests: Option<u32>, token_budget: Option<u64>, cost_budget: Option<f64>) -> Self {
        Self {
            max_model_requests,
            token_budget,
            cost_budget,
            ..Self::default()
        }
    }

    /// Count `requests` model requests and the usage of their responses.
    pub(crate) fn count_requests(&mut self, requests: u32, usage: &Usage) {
        self.model_requests = self.model_requests.saturating_add(requests);
        self.usage.add(usage);
    }

    /// Count the usage the tools reported with `results`.
    pub(crate) fn count_tool_usage(&mut self, results: &[ToolResultMessage]) {
        for result in results {
            if let Some(usage) = &result.usage {
                self.tool_usage.entry(result.tool_name.clone()).or_default().add(usage);
            }
        }
    }

    /// Model requests the run may still make; `None` is unlimited.
    pub(crate) fn remaining_model_requests(&self) -> Option<u32> {
        self.max_model_requests
            .map(|limit| limit.saturating_sub(self.model_requests))
    }

    /// The outcome when a budget leaves no room for another request: requests first, then
    /// tokens, then cost. Checked before each request, so the last one may go over the
    /// token or the cost budget.
    pub(crate) fn exhausted(&self) -> Option<RunOutcome> {
        if self
            .max_model_requests
            .is_some_and(|limit| self.model_requests >= limit)
        {
            return Some(RunOutcome::ModelRequestLimit);
        }
        if self
            .token_budget
            .is_some_and(|budget| self.usage.input_and_output() >= budget)
        {
            return Some(RunOutcome::TokenBudget);
        }
        if self
            .cost_budget
            .is_some_and(|budget| self.usage.total_cost.unwrap_or(0.0) >= budget)
        {
            return Some(RunOutcome::CostBudget);
        }
        None
    }
}
