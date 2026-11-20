//! Retry decisions and backoff (pi: packages/ai/src/utils/retry.ts; the jitter follows
//! provider-retry.ts; the defaults are those of coding-agent/src/core/settings-defaults.ts).
//!
//! What went wrong is the provider's to say, as the [`ProviderErrorKind`] of a failed
//! message; whether to ask again, or to compact a context that overflowed instead, is decided
//! here from that kind alone.

use std::time::Duration;

use zx_util::random::random_fraction;

use super::types::{AssistantMessage, ProviderErrorKind, StopReason};

/// How often and how patiently to retry failed responses.
#[derive(Clone, Debug)]
pub struct RetryPolicy {
    /// Retries after the first attempt. 0 disables retrying.
    pub max_retries: u32,
    /// The delay before the first retry, doubled for each one after it.
    pub base_delay: Duration,
    /// The longest wait before a retry, whatever the backoff or the server's hint says.
    pub max_delay: Duration,
    /// How much of a backoff is taken off at random, at most: 0.25 shortens each wait by up
    /// to a quarter, so that clients that failed together do not retry together. 0 waits
    /// exactly the backoff. A server's `Retry-After` is kept as it is.
    pub jitter: f64,
}

impl Default for RetryPolicy {
    /// 3 retries, a 2 s base delay, a 60 s cap and up to a quarter of jitter.
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(60),
            jitter: 0.25,
        }
    }
}

impl RetryPolicy {
    /// The nominal delay before the `retry`th retry (1-based): `base * 2^(retry-1)`, capped.
    fn delay(&self, retry: u32) -> Duration {
        let factor = 2u32.saturating_pow(retry.saturating_sub(1));
        self.base_delay.saturating_mul(factor).min(self.max_delay)
    }

    /// The wait before the `retry`th retry: the nominal delay with jitter taken off, or the
    /// server's `Retry-After` when that is longer; never above `max_delay`.
    pub fn delay_for(&self, retry: u32, message: &AssistantMessage) -> Duration {
        let hint = message
            .provider_error
            .as_ref()
            .and_then(|error| error.retry_after_ms)
            .map_or(Duration::ZERO, Duration::from_millis);
        self.jittered(self.delay(retry)).max(hint).min(self.max_delay)
    }

    /// `delay` shortened by a random share of at most `jitter`; a jitter that is not a number
    /// takes nothing off, as `clamp` would keep it and `mul_f64` panic on it.
    fn jittered(&self, delay: Duration) -> Duration {
        let jitter = if self.jitter.is_nan() {
            0.0
        } else {
            self.jitter.clamp(0.0, 1.0)
        };
        let share = jitter * random_fraction();
        delay.mul_f64(1.0 - share)
    }
}

/// The kind of failure that ended `message`: `None` when it did not fail, was cancelled, or
/// failed in a way its provider did not classify.
fn failure_kind(message: &AssistantMessage) -> Option<ProviderErrorKind> {
    if message.stop_reason != StopReason::Error {
        return None;
    }
    message.provider_error.as_ref().map(|error| error.kind)
}

/// Whether asking again may get past a failed response: one that was rate limited, found the
/// service unavailable, lost its connection or timed out. Nothing else is retried, a failure
/// its provider did not classify included.
pub fn is_retryable(message: &AssistantMessage) -> bool {
    match failure_kind(message) {
        Some(
            ProviderErrorKind::RateLimited
            | ProviderErrorKind::Unavailable
            | ProviderErrorKind::Transport
            | ProviderErrorKind::Timeout,
        ) => true,
        Some(
            ProviderErrorKind::ContextOverflow
            | ProviderErrorKind::AccessDenied
            | ProviderErrorKind::ApiKey
            | ProviderErrorKind::Refused
            | ProviderErrorKind::Protocol
            | ProviderErrorKind::ResourceLimit
            | ProviderErrorKind::Other,
        )
        | None => false,
    }
}

/// Whether a failed response says the request was too long for the model's context
/// ([`ProviderErrorKind::ContextOverflow`]): a smaller context may fix it, repeating the
/// request never does. Silent overflow, where a service truncates the input instead of
/// failing, is not detected.
pub fn is_context_overflow(message: &AssistantMessage) -> bool {
    failure_kind(message) == Some(ProviderErrorKind::ContextOverflow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProviderError;

    /// A failed message whose provider reported `error`.
    fn failed(error: impl Into<ProviderError>) -> AssistantMessage {
        let mut message = AssistantMessage::failure("p", "m", StopReason::Error, "failed");
        message.provider_error = Some(error.into());
        message
    }

    /// Asked for a retry after `retry_after_ms`, as a rate limited response says.
    fn hinted(retry_after_ms: u64) -> AssistantMessage {
        failed(ProviderError {
            status: Some(429),
            retry_after_ms: Some(retry_after_ms),
            ..ProviderError::from(ProviderErrorKind::RateLimited)
        })
    }

    fn exact() -> RetryPolicy {
        RetryPolicy {
            jitter: 0.0,
            ..RetryPolicy::default()
        }
    }

    const EVERY_KIND: [ProviderErrorKind; 11] = [
        ProviderErrorKind::RateLimited,
        ProviderErrorKind::Unavailable,
        ProviderErrorKind::Transport,
        ProviderErrorKind::Timeout,
        ProviderErrorKind::ContextOverflow,
        ProviderErrorKind::AccessDenied,
        ProviderErrorKind::ApiKey,
        ProviderErrorKind::Refused,
        ProviderErrorKind::Protocol,
        ProviderErrorKind::ResourceLimit,
        ProviderErrorKind::Other,
    ];

    #[test]
    fn a_jitter_that_is_not_a_number_takes_nothing_off() {
        let policy = RetryPolicy {
            jitter: f64::NAN,
            ..RetryPolicy::default()
        };
        assert_eq!(
            policy.delay_for(1, &failed(ProviderErrorKind::Unavailable)),
            policy.delay(1)
        );
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.delay(1), Duration::from_secs(2));
        assert_eq!(policy.delay(2), Duration::from_secs(4));
        assert_eq!(policy.delay(3), Duration::from_secs(8));
        assert_eq!(policy.delay(30), Duration::from_secs(60));
    }

    #[test]
    fn jitter_shortens_a_wait_by_at_most_its_share_and_never_a_server_hint() {
        let policy = RetryPolicy::default();
        let unavailable = failed(ProviderErrorKind::Unavailable);
        let nominal = policy.delay(3);
        let mut waits = Vec::new();
        for _ in 0..64 {
            let wait = policy.delay_for(3, &unavailable);
            assert!(wait <= nominal && wait >= nominal.mul_f64(0.75), "{wait:?}");
            waits.push(wait);
        }
        assert!(waits.iter().any(|wait| *wait != nominal), "the waits vary");
        assert_eq!(policy.delay_for(1, &hinted(7000)), Duration::from_secs(7));
        assert_eq!(exact().delay_for(1, &unavailable), Duration::from_secs(2));
    }

    #[test]
    fn server_retry_hints_respect_the_configured_delay_cap() {
        let policy = exact();
        assert_eq!(policy.delay_for(1, &hinted(7000)), Duration::from_secs(7));
        assert_eq!(policy.delay_for(3, &hinted(7000)), Duration::from_secs(8));
        assert_eq!(policy.delay_for(1, &hinted(120_000)), policy.max_delay);
    }

    #[test]
    fn only_rate_limits_unavailability_lost_connections_and_timeouts_are_retried() {
        let retried: Vec<ProviderErrorKind> = EVERY_KIND
            .into_iter()
            .filter(|kind| is_retryable(&failed(*kind)))
            .collect();
        assert_eq!(
            retried,
            [
                ProviderErrorKind::RateLimited,
                ProviderErrorKind::Unavailable,
                ProviderErrorKind::Transport,
                ProviderErrorKind::Timeout
            ]
        );
    }

    #[test]
    fn only_a_context_overflow_is_one() {
        let overflows: Vec<ProviderErrorKind> = EVERY_KIND
            .into_iter()
            .filter(|kind| is_context_overflow(&failed(*kind)))
            .collect();
        assert_eq!(overflows, [ProviderErrorKind::ContextOverflow]);
    }

    #[test]
    fn a_cancelled_or_unclassified_failure_is_neither_retried_nor_an_overflow() {
        for kind in [ProviderErrorKind::Unavailable, ProviderErrorKind::ContextOverflow] {
            let mut cancelled = failed(kind);
            cancelled.stop_reason = StopReason::Aborted;
            assert!(!is_retryable(&cancelled), "{kind:?}");
            assert!(!is_context_overflow(&cancelled), "{kind:?}");
        }
        let unclassified = AssistantMessage::failure("p", "m", StopReason::Error, "HTTP 503: overloaded");
        assert!(!is_retryable(&unclassified));
        assert!(!is_context_overflow(&unclassified));
    }
}
