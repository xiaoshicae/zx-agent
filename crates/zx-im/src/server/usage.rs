//! What the work of a day may use, in all and in each chat, and what it used: counted as each
//! piece of work ends, and started over as the day changes, at 00:00 UTC.

use std::collections::HashMap;

use zx_agent::ai::{Usage, now_ms};

use crate::ConversationId;

/// Milliseconds in a day.
const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// What the work of a day may use, from 00:00 UTC: in all ([`ServerConfig::daily`]) or in one
/// chat ([`ChatConfig::daily`]). `None` does not limit. New work starts only while neither is
/// used up; work that has started finishes, so a day may go over by what the work in progress
/// uses, within each run's own budgets.
///
/// [`ServerConfig::daily`]: super::ServerConfig::daily
/// [`ChatConfig::daily`]: super::ChatConfig::daily
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DailyLimit {
    /// Input plus output tokens, as the providers report them.
    pub tokens: Option<u64>,
    /// What the work may cost, in the unit of the configured prices. A response of a model
    /// without prices costs nothing here.
    pub cost: Option<f64>,
}

impl DailyLimit {
    /// Whether `used` leaves nothing of this limit.
    pub(super) fn reached_by(self, used: Used) -> bool {
        self.tokens.is_some_and(|tokens| used.tokens >= tokens) || self.cost.is_some_and(|cost| used.cost >= cost)
    }
}

/// What some work used: its tokens, input plus output, and its cost.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Used {
    tokens: u64,
    cost: f64,
}

impl Used {
    fn add(&mut self, usage: &Usage) {
        self.tokens = self.tokens.saturating_add(usage.input_and_output());
        // Unpriced, it costs nothing: the program asks for prices with a daily cost.
        self.cost += usage.total_cost.unwrap_or(0.0);
    }
}

/// What the work of one day used, in all and by chat, which `day` says.
#[derive(Default)]
pub(super) struct DayUsage {
    /// Days since 1970-01-01, UTC.
    day: u64,
    all: Used,
    chats: HashMap<(String, ConversationId), Used>,
}

impl DayUsage {
    /// What the work of `day` used so far: in all, and in the chat `chat` on `platform`.
    pub(super) fn used(&mut self, day: u64, platform: &str, chat: &ConversationId) -> (Used, Used) {
        self.go_to(day);
        let in_chat = self
            .chats
            .get(&(platform.to_string(), chat.clone()))
            .copied()
            .unwrap_or_default();
        (self.all, in_chat)
    }

    /// Count `usage`, what work in the chat `chat` on `platform` used on `day`.
    pub(super) fn add(&mut self, day: u64, platform: &str, chat: &ConversationId, usage: &Usage) {
        self.go_to(day);
        self.all.add(usage);
        let key = (platform.to_string(), chat.clone());
        self.chats.entry(key).or_default().add(usage);
    }

    /// Start `day` over when another day was counted so far.
    fn go_to(&mut self, day: u64) {
        if self.day != day {
            *self = Self { day, ..Self::default() };
        }
    }
}

/// Today, as days since 1970-01-01, UTC.
pub(super) fn today() -> u64 {
    now_ms() / DAY_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(tokens: u64, cost: Option<f64>) -> Usage {
        Usage {
            input: tokens,
            total_cost: cost,
            ..Usage::default()
        }
    }

    fn chat(id: &str) -> ConversationId {
        ConversationId(id.into())
    }

    #[test]
    fn work_counts_in_all_and_in_its_chat_until_the_day_changes() {
        let mut usage_of_day = DayUsage::default();
        usage_of_day.add(7, "slack", &chat("C1"), &usage(100, Some(0.5)));
        usage_of_day.add(7, "slack", &chat("C2"), &usage(50, None));
        let (all, in_c1) = usage_of_day.used(7, "slack", &chat("C1"));
        assert_eq!(all, Used { tokens: 150, cost: 0.5 });
        assert_eq!(in_c1, Used { tokens: 100, cost: 0.5 });
        // The same ID on another platform is another chat.
        assert_eq!(usage_of_day.used(7, "feishu", &chat("C1")).1, Used::default());
        // A new day starts from nothing.
        assert_eq!(
            usage_of_day.used(8, "slack", &chat("C1")),
            (Used::default(), Used::default())
        );
    }

    #[test]
    fn a_limit_is_reached_once_used_up_and_none_never_is() {
        let used = Used { tokens: 100, cost: 0.5 };
        let tokens = |limit| DailyLimit {
            tokens: Some(limit),
            cost: None,
        };
        assert!(tokens(100).reached_by(used));
        assert!(!tokens(101).reached_by(used));
        let cost = |limit| DailyLimit {
            tokens: None,
            cost: Some(limit),
        };
        assert!(cost(0.5).reached_by(used));
        assert!(!cost(0.51).reached_by(used));
        assert!(!DailyLimit::default().reached_by(used));
    }
}
