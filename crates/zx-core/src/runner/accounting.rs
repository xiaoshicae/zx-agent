//! What a run spends across its attempts, and the budgets it spends against. The run owns
//! the accounting and lends it to the loop of each attempt, which counts every request,
//! response and tool call as it happens and checks the budgets before each request; the
//! run reads the totals for its report.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use super::RunLimits;
use crate::ai::{ModelMeta, Usage};
use crate::events::Budget;
use crate::text::amount;

/// What a run has spent so far, over all its attempts, and its budgets.
#[derive(Default)]
pub(crate) struct RunAccounting {
    /// Model requests the run may make, summary requests included; `None` is unlimited,
    /// `Some(0)` makes no request.
    max_model_requests: Option<u32>,
    /// Tokens the run may use, input plus output as the provider reports them, summary
    /// requests included; `None` is unlimited.
    token_budget: Option<u64>,
    /// What the run may spend, in the unit of the configured prices. With a budget,
    /// requests whose models have no prices are refused. `None` is unlimited.
    cost_budget: Option<f64>,
    /// Attempts started: the first, retries and recoveries from context overflow. The
    /// attempt in progress is the last one.
    pub(crate) attempts: u32,
    /// Task steps started; the step in progress is the last one, so step numbers go on
    /// across attempts.
    pub(crate) task_steps: u32,
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
    /// Automatic compactions before a request that left the context over its trigger, failed
    /// or with nothing to summarize: after one, the run goes on with its context as it is and
    /// does not compact before its later requests.
    pub(crate) failed_compactions: u32,
}

/// Refuse a request to `model` whose cost `cost_budget` could not count: with a budget, every
/// model a run asks, for a task or a summary, has prices in `models`.
pub(super) fn check_priced(
    models: &HashMap<String, ModelMeta>,
    model: &str,
    cost_budget: Option<f64>,
) -> Result<(), String> {
    if cost_budget.is_some() && !models.get(model).is_some_and(|meta| meta.cost.is_some()) {
        return Err(format!(
            "a cost budget needs prices for model {model:?}; configure its model metadata before making requests"
        ));
    }
    Ok(())
}

/// A budget that leaves no room for another request, with what the run spent against it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Exhausted {
    /// The run made `max_model_requests` requests.
    ModelRequests { max_model_requests: u32 },
    /// The run used `used` tokens of `token_budget`.
    Tokens { used: u64, token_budget: u64 },
    /// The run spent `spent` of `cost_budget`.
    Cost { spent: f64, cost_budget: f64 },
}

impl Exhausted {
    /// The budget, as events and reports name it.
    pub(crate) fn budget(self) -> Budget {
        match self {
            Self::ModelRequests { .. } => Budget::ModelRequests,
            Self::Tokens { .. } => Budget::Tokens,
            Self::Cost { .. } => Budget::Cost,
        }
    }
}

impl fmt::Display for Exhausted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ModelRequests { max_model_requests } => write!(
                formatter,
                "model request limit reached (max_model_requests = {max_model_requests})"
            ),
            Self::Tokens { used, token_budget } => write!(
                formatter,
                "token budget reached ({used} tokens used, token_budget = {token_budget})"
            ),
            Self::Cost { spent, cost_budget } => write!(
                formatter,
                "cost budget reached ({} spent, cost_budget = {cost_budget})",
                amount(*spent)
            ),
        }
    }
}

impl RunAccounting {
    /// Nothing spent yet, against the budgets of `limits`.
    pub(crate) fn new(limits: &RunLimits) -> Self {
        Self {
            max_model_requests: limits.max_model_requests,
            token_budget: limits.token_budget,
            cost_budget: limits.cost_budget,
            ..Self::default()
        }
    }

    /// Count `requests` model requests and the usage of their responses.
    pub(crate) fn count_requests(&mut self, requests: u32, usage: &Usage) {
        self.model_requests = self.model_requests.saturating_add(requests);
        self.usage.add(usage);
    }

    /// Count `usage`, which the tool called `tool` reported with a result.
    pub(crate) fn count_tool_usage(&mut self, tool: &str, usage: &Usage) {
        self.tool_usage.entry(tool.to_string()).or_default().add(usage);
    }

    /// Model requests the run may still make; `None` is unlimited.
    pub(crate) fn remaining_model_requests(&self) -> Option<u32> {
        self.max_model_requests
            .map(|limit| limit.saturating_sub(self.model_requests))
    }

    /// The budget that leaves no room for another request, if one does: requests first, then
    /// tokens, then cost. Checked before each request, so the last one may go over the token
    /// or the cost budget.
    pub(crate) fn exhausted(&self) -> Option<Exhausted> {
        if let Some(max_model_requests) = self.max_model_requests
            && self.model_requests >= max_model_requests
        {
            return Some(Exhausted::ModelRequests { max_model_requests });
        }
        let used = self.usage.input_and_output();
        if let Some(token_budget) = self.token_budget
            && used >= token_budget
        {
            return Some(Exhausted::Tokens { used, token_budget });
        }
        // Nothing priced yet is nothing spent.
        let spent = self.usage.total_cost.unwrap_or(0.0);
        if let Some(cost_budget) = self.cost_budget
            && spent >= cost_budget
        {
            return Some(Exhausted::Cost { spent, cost_budget });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reached_cost_budget_shows_a_small_spending_to_its_first_two_digits() {
        let reached = Exhausted::Cost {
            spent: 0.000_062_1,
            cost_budget: 0.000_05,
        };

        assert_eq!(
            reached.to_string(),
            "cost budget reached (0.000062 spent, cost_budget = 0.00005)"
        );
    }
}
