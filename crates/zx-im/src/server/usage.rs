//! What the work of a day may use, in all and in each chat, and what it used: counted as each
//! piece of work ends, started over as the day changes, at 00:00 UTC, and kept in a store when
//! the server has one, so that a restart, or another replica, counts the day on.

use std::collections::HashMap;
use std::num::NonZeroU64;

use async_trait::async_trait;
use zx_agent::ai::{Usage, now_ms};

use crate::ConversationId;

/// Milliseconds in a day.
const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// What the work of a day may use, from 00:00 UTC: in all ([`ServerConfig::daily`]) or in one
/// chat ([`ChatConfig::daily`]). [`Default`] does not limit. New work starts only while
/// neither is used up; work that has started finishes, so a day may go over by what the work
/// in progress uses, within each run's own budgets.
///
/// [`ServerConfig::daily`]: super::ServerConfig::daily
/// [`ChatConfig::daily`]: super::ChatConfig::daily
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DailyLimit {
    tokens: Option<NonZeroU64>,
    /// A finite amount above zero, as [`DailyLimit::new`] takes it.
    cost: Option<f64>,
}

impl DailyLimit {
    /// A limit of `tokens`, input plus output as the providers report them, and of `cost`,
    /// what the work may cost in the unit of the configured prices; `None` does not limit
    /// that one. A response of a model without prices costs nothing here.
    ///
    /// An error for a cost that would not limit as a limit does: one that is not a number or
    /// is infinite is never reached, and zero or less is reached before any work.
    pub fn new(tokens: Option<NonZeroU64>, cost: Option<f64>) -> Result<Self, InvalidDailyLimit> {
        match cost {
            Some(cost) if !(cost.is_finite() && cost > 0.0) => Err(InvalidDailyLimit::Cost(cost)),
            _ => Ok(Self { tokens, cost }),
        }
    }

    /// The tokens a day may use, input plus output; `None` when it does not limit them.
    pub fn tokens(self) -> Option<NonZeroU64> {
        self.tokens
    }

    /// What a day may cost; `None` when it does not limit the cost.
    pub fn cost(self) -> Option<f64> {
        self.cost
    }

    /// Whether `used` leaves nothing of this limit.
    pub(super) fn reached_by(self, used: Used) -> bool {
        self.tokens.is_some_and(|tokens| used.tokens >= tokens.get()) || self.cost.is_some_and(|cost| used.cost >= cost)
    }
}

/// Why [`DailyLimit::new`] refuses a limit. The message names the parameter as the
/// constructor does; a host that reads the limit from a file of its own matches the variant
/// to name its own key.
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
pub enum InvalidDailyLimit {
    /// `cost`, given here, is zero or less, or not a finite number.
    #[error("cost is {0}; it must be a finite amount above zero")]
    Cost(f64),
}

/// What the work in one chat used, on one day or in one piece of work.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatUsage {
    pub platform: String,
    pub chat: ConversationId,
    /// Input plus output tokens, as the providers report them.
    pub tokens: u64,
    /// In the unit of the configured prices; responses without prices cost nothing here.
    pub cost: f64,
}

impl ChatUsage {
    /// What `usage`, the usage of work in the chat `chat` on `platform`, counts.
    pub(super) fn of(platform: &str, chat: &ConversationId, usage: &Usage) -> Self {
        Self {
            platform: platform.to_string(),
            chat: chat.clone(),
            tokens: usage.input_and_output(),
            // Unpriced, it costs nothing: the program asks for prices with a daily cost.
            cost: usage.total_cost.unwrap_or(0.0),
        }
    }
}

/// Keeps what the work of each day used ([`ServerConfig::usage`]), so that a restart, or the
/// replica that takes over, counts the day on.
///
/// [`ServerConfig::usage`]: super::ServerConfig::usage
#[async_trait]
pub trait UsageStore: Send + Sync {
    /// What the work of `day`, in days since 1970-01-01 UTC, used so far: each chat's.
    async fn day(&self, day: u64) -> Result<Vec<ChatUsage>, String>;

    /// Add `used`, what work in its chat used, to what `day` used there.
    async fn add(&self, day: u64, used: &ChatUsage) -> Result<(), String>;
}

/// What some work used: its tokens, input plus output, and its cost.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Used {
    tokens: u64,
    cost: f64,
}

impl Used {
    fn add(&mut self, used: &ChatUsage) {
        self.tokens = self.tokens.saturating_add(used.tokens);
        self.cost += used.cost;
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

    /// Count `used`, what work in its chat used on `day`.
    pub(super) fn add(&mut self, day: u64, used: &ChatUsage) {
        self.go_to(day);
        self.all.add(used);
        let key = (used.platform.clone(), used.chat.clone());
        self.chats.entry(key).or_default().add(used);
    }

    /// Start `day` over from what a store kept of it, `kept`.
    pub(super) fn load(&mut self, day: u64, kept: &[ChatUsage]) {
        *self = Self { day, ..Self::default() };
        for used in kept {
            self.add(day, used);
        }
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

    fn in_chat(platform: &str, chat: &str, tokens: u64, cost: f64) -> ChatUsage {
        ChatUsage {
            platform: platform.into(),
            chat: ConversationId(chat.into()),
            tokens,
            cost,
        }
    }

    fn chat(id: &str) -> ConversationId {
        ConversationId(id.into())
    }

    #[test]
    fn work_counts_in_all_and_in_its_chat_until_the_day_changes() {
        let mut usage_of_day = DayUsage::default();
        usage_of_day.add(7, &in_chat("slack", "C1", 100, 0.5));
        usage_of_day.add(7, &in_chat("slack", "C2", 50, 0.0));
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
    fn a_day_a_store_kept_counts_on_from_what_it_holds() {
        let mut usage_of_day = DayUsage::default();
        usage_of_day.add(6, &in_chat("slack", "C1", 999, 9.0));
        usage_of_day.load(7, &[in_chat("slack", "C1", 100, 0.5), in_chat("slack", "C2", 50, 0.0)]);
        usage_of_day.add(7, &in_chat("slack", "C1", 10, 0.25));
        let (all, in_c1) = usage_of_day.used(7, "slack", &chat("C1"));
        assert_eq!(
            all,
            Used {
                tokens: 160,
                cost: 0.75
            }
        );
        assert_eq!(
            in_c1,
            Used {
                tokens: 110,
                cost: 0.75
            }
        );
    }

    #[test]
    fn work_is_counted_by_its_tokens_in_and_out_and_its_price() {
        let usage = Usage {
            input: 10,
            output: 5,
            total_cost: Some(0.5),
            ..Usage::default()
        };
        assert_eq!(
            ChatUsage::of("slack", &chat("C1"), &usage),
            in_chat("slack", "C1", 15, 0.5)
        );
        let unpriced = Usage {
            total_cost: None,
            ..usage
        };
        assert_eq!(ChatUsage::of("slack", &chat("C1"), &unpriced).cost, 0.0);
    }

    #[test]
    fn a_limit_is_reached_once_used_up_and_none_never_is() {
        let used = Used { tokens: 100, cost: 0.5 };
        let tokens = |limit| DailyLimit::new(NonZeroU64::new(limit), None).unwrap();
        assert!(tokens(100).reached_by(used));
        assert!(!tokens(101).reached_by(used));
        let cost = |limit| DailyLimit::new(None, Some(limit)).unwrap();
        assert!(cost(0.5).reached_by(used));
        assert!(!cost(0.51).reached_by(used));
        assert!(!DailyLimit::default().reached_by(used));
    }

    #[test]
    fn a_cost_that_is_no_finite_amount_above_zero_is_not_a_limit() {
        for cost in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let limit = DailyLimit::new(None, Some(cost));
            assert!(matches!(limit, Err(InvalidDailyLimit::Cost(_))), "{cost}: {limit:?}");
        }
        assert_eq!(
            DailyLimit::new(None, Some(-1.0)).unwrap_err().to_string(),
            "cost is -1; it must be a finite amount above zero"
        );
    }
}
