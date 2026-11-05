//! Aggregate attempt costs and calculate the remaining run budget.

use super::Limits;
use crate::agent::LoopResult;
use crate::ai::Usage;

#[derive(Default)]
pub(super) struct RunAccounting {
    pub task_turns: u32,
    pub model_requests: u32,
    pub retries: u32,
    pub answer_continuations: u32,
    pub usage: Usage,
}

impl RunAccounting {
    pub fn record(&mut self, attempt: &LoopResult) {
        self.task_turns = self.task_turns.saturating_add(attempt.task_turns);
        self.model_requests = self.model_requests.saturating_add(attempt.model_requests);
        self.answer_continuations = attempt.answer_continuations;
        self.usage.add(&attempt.usage);
    }

    pub fn remaining_model_requests(&self, limits: &Limits) -> Option<u32> {
        limits
            .max_model_requests
            .map(|limit| limit.saturating_sub(self.model_requests))
    }

    pub fn remaining_tokens(&self, limits: &Limits) -> Option<u64> {
        limits
            .token_budget
            .map(|limit| limit.saturating_sub(self.usage.input_and_output()))
    }

    pub fn has_budget(&self, limits: &Limits) -> bool {
        self.remaining_model_requests(limits) != Some(0) && self.remaining_tokens(limits) != Some(0)
    }
}
