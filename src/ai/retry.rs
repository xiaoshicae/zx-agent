//! Retry classification and backoff (pi: packages/ai/src/utils/retry.ts).

use std::time::Duration;

use super::types::{AssistantMessage, StopReason};

/// How often and how patiently to retry failed responses.
#[derive(Clone, Debug)]
pub struct RetryPolicy {
    /// Retries after the first attempt. 0 disables retrying.
    pub max_retries: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    /// pi defaults: 3 retries, 2s base delay, 60s cap (coding-agent/src/core/settings-defaults.ts).
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(60),
        }
    }
}

impl RetryPolicy {
    /// Delay before retry `attempt` (1-based): `base * 2^(attempt-1)`, capped.
    pub fn delay(&self, attempt: u32) -> Duration {
        let factor = 2u32.saturating_pow(attempt.saturating_sub(1));
        self.base_delay.saturating_mul(factor).min(self.max_delay)
    }
}

/// Failures that retrying cannot fix: credentials, quota, and context size.
const NON_RETRYABLE: &[&str] = &[
    "401",
    "403",
    "unauthorized",
    "invalid api key",
    "invalid_api_key",
    "insufficient_quota",
    "billing",
    "context length",
    "context_length",
    "maximum context",
    "too many tokens",
];

/// Transient provider and transport failures.
const RETRYABLE: &[&str] = &[
    "429",
    "rate limit",
    "rate_limit",
    "too many requests",
    "overloaded",
    "server_busy",
    "500",
    "502",
    "503",
    "504",
    "internal server error",
    "bad gateway",
    "service unavailable",
    "gateway timeout",
    "timed out",
    "stream stalled",
    "stream read failed",
    "request failed",
    "connection",
    "temporarily",
];

/// Whether a failed response looks transient. Like pi, this matches error text:
/// providers do not report a common machine-readable "retryable" flag.
pub fn is_retryable(message: &AssistantMessage) -> bool {
    if message.stop_reason != StopReason::Error {
        return false;
    }
    let Some(error) = message.error_message.as_deref() else {
        return false;
    };
    let error = error.to_lowercase();
    if NON_RETRYABLE.iter().any(|pattern| mentions(&error, pattern)) {
        return false;
    }
    RETRYABLE.iter().any(|pattern| mentions(&error, pattern))
}

/// Whether `error` contains `pattern`. A number must stand alone, with no letter or digit
/// right before or after it, so status codes do not match inside request ids such as
/// `9d500a1b` (pi: `(?<![a-z0-9])(?:429|500|...)(?![a-z0-9])` in retry.ts).
fn mentions(error: &str, pattern: &str) -> bool {
    if !pattern.bytes().all(|byte| byte.is_ascii_digit()) {
        return error.contains(pattern);
    }
    error.match_indices(pattern).any(|(start, _)| {
        let before = error[..start].chars().next_back();
        let after = error[start + pattern.len()..].chars().next();
        ![before, after].into_iter().flatten().any(|c| c.is_ascii_alphanumeric())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(error: &str) -> AssistantMessage {
        AssistantMessage::failure("p", "m", StopReason::Error, error)
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
    fn status_codes_match_only_as_whole_numbers() {
        // Request ids are hex strings, which can contain digits such as "500" or "401".
        assert!(!is_retryable(&failed(
            "HTTP 400: unknown model (request_id: 9d500a1b-0c2e)"
        )));
        assert!(is_retryable(&failed("HTTP 503: busy (request_id: 1401ab-77)")));
    }

    #[test]
    fn classifies_errors() {
        assert!(is_retryable(&failed("HTTP 503: service unavailable")));
        assert!(is_retryable(&failed("HTTP 529: overloaded_error")));
        assert!(is_retryable(&failed("stream stalled: no data for 120s")));
        assert!(!is_retryable(&failed("HTTP 401: invalid api key")));
        assert!(!is_retryable(&failed("HTTP 429: insufficient_quota")));
        assert!(!is_retryable(&failed("HTTP 400: context_length_exceeded")));
        assert!(!is_retryable(&AssistantMessage::failure(
            "p",
            "m",
            StopReason::Aborted,
            "503"
        )));
    }
}
