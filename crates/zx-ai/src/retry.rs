//! Retry classification and backoff (pi: packages/ai/src/utils/retry.ts and overflow.ts; the
//! jitter follows provider-retry.ts; the defaults are those of
//! coding-agent/src/core/settings-defaults.ts).

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::time::Duration;

use super::types::{AssistantMessage, ProviderError, StopReason};

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
        let hint = match &message.provider_error {
            Some(ProviderError::Http {
                retry_after_ms: Some(ms),
                ..
            }) => Duration::from_millis(*ms),
            _ => Duration::ZERO,
        };
        self.jittered(self.delay(retry)).max(hint).min(self.max_delay)
    }

    /// `delay` shortened by a random share of at most `jitter`.
    fn jittered(&self, delay: Duration) -> Duration {
        let share = self.jitter.clamp(0.0, 1.0) * random_fraction();
        delay.mul_f64(1.0 - share)
    }
}

/// A number in `[0, 1)` from the standard library's randomly keyed hasher: enough to spread
/// retries out, without a random number generator among the dependencies.
fn random_fraction() -> f64 {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(0x5eed);
    // The top 53 bits of the hash make an exact double in [0, 1).
    (hasher.finish() >> 11) as f64 / (1u64 << 53) as f64
}

/// Failures that retrying cannot fix: credentials and quota. A request too long for the
/// model is permanent too; its wording is [`OVERFLOW_TEXT`].
const PERMANENT_TEXT: &[&str] = &[
    "401",
    "403",
    "unauthorized",
    "invalid api key",
    "invalid_api_key",
    "insufficient_quota",
    "billing",
];

/// Transient provider and transport failures other than rate limiting, whose wording is
/// [`RATE_LIMIT_TEXT`].
const TRANSIENT_TEXT: &[&str] = &[
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

/// Whether a failed response is transient. Structured metadata takes precedence; messages
/// without it, from providers that report none, use the text fallback.
pub fn is_retryable(message: &AssistantMessage) -> bool {
    if message.stop_reason != StopReason::Error {
        return false;
    }
    match &message.provider_error {
        Some(ProviderError::Http { status, code, .. }) => {
            let permanent = code.as_deref().is_some_and(permanent_code);
            return !permanent && matches!(status, 408 | 429 | 500..=599);
        }
        Some(ProviderError::Provider { code: Some(code) }) => {
            return matches!(
                code.to_ascii_lowercase().as_str(),
                "rate_limit_exceeded"
                    | "rate_limit_error"
                    | "server_error"
                    | "internal_error"
                    | "overloaded_error"
                    | "server_busy"
                    | "timeout"
            );
        }
        Some(ProviderError::Transport | ProviderError::Timeout) => return true,
        // A key source that fails needs the host's attention, not another try.
        Some(ProviderError::Protocol | ProviderError::ResourceLimit | ProviderError::ApiKey) => return false,
        Some(ProviderError::Provider { code: None }) | None => {}
    }
    let Some(error) = message.error_message.as_deref() else {
        return false;
    };
    let error = error.to_lowercase();
    if mentions_any(&error, PERMANENT_TEXT) || overflow_text(&error) {
        return false;
    }
    mentions_any(&error, TRANSIENT_TEXT) || mentions_any(&error, RATE_LIMIT_TEXT)
}

/// Error codes that say the request was too long for the model. Codes, not vendor names: any
/// service may use them.
const OVERFLOW_CODES: &[&str] = &[
    "context_length_exceeded",
    "model_context_window_exceeded",
    "request_too_large",
];

/// Phrases that say the request was too long for the model, for failures without such a code.
/// Generic wording only, not one message per service.
const OVERFLOW_TEXT: &[&str] = &[
    "context length",
    "context_length",
    "maximum context",
    "context window",
    "context_window",
    "context size",
    "prompt is too long",
    "prompt too long",
    "request_too_large",
    "too many tokens",
];

/// Rate limiting, which a retry may outlast and which can also mention tokens ("too many
/// tokens, please wait"), so that it is never taken for an overflow.
const RATE_LIMIT_TEXT: &[&str] = &["429", "rate limit", "rate_limit", "too many requests"];

/// Whether a failed response says the request was too long for the model's context.
///
/// A smaller context may fix it, repeating the request never does. An HTTP 413 or one of the
/// overflow codes decides; throttling, server, credential and transport failures never count;
/// otherwise the error text decides. Silent overflow, where a service truncates the input
/// instead of failing, is not detected.
pub fn is_context_overflow(message: &AssistantMessage) -> bool {
    if message.stop_reason != StopReason::Error {
        return false;
    }
    match &message.provider_error {
        Some(ProviderError::Http { status: 413, .. }) => return true,
        Some(ProviderError::Http { status, code, .. }) => {
            if code.as_deref().is_some_and(overflow_code) {
                return true;
            }
            if matches!(status, 401 | 403 | 408 | 429 | 500..=599) {
                return false;
            }
        }
        Some(ProviderError::Provider { code: Some(code) }) if overflow_code(code) => return true,
        Some(
            ProviderError::Transport
            | ProviderError::Timeout
            | ProviderError::Protocol
            | ProviderError::ResourceLimit
            | ProviderError::ApiKey,
        ) => {
            return false;
        }
        Some(ProviderError::Provider { .. }) | None => {}
    }
    let Some(error) = message.error_message.as_deref() else {
        return false;
    };
    let error = error.to_lowercase();
    // Throttling, including a `ThrottlingException`, which the text fallback of
    // `is_retryable` does not know: whether to retry on it is a separate decision.
    if mentions_any(&error, RATE_LIMIT_TEXT) || error.contains("throttl") {
        return false;
    }
    error.starts_with("http 413") || overflow_text(&error)
}

fn overflow_code(code: &str) -> bool {
    OVERFLOW_CODES.contains(&code.to_ascii_lowercase().as_str())
}

/// Whether the lowercased `error` has one of the overflow phrases.
fn overflow_text(error: &str) -> bool {
    OVERFLOW_TEXT.iter().any(|pattern| error.contains(pattern))
}

/// Error codes that a retry cannot get past: credentials, quota, and the overflow codes.
fn permanent_code(code: &str) -> bool {
    overflow_code(code)
        || matches!(
            code.to_ascii_lowercase().as_str(),
            "insufficient_quota"
                | "billing_hard_limit_reached"
                | "invalid_api_key"
                | "authentication_error"
                | "permission_error"
        )
}

/// Whether the lowercased `error` mentions one of `patterns`, as [`mentions`] decides.
fn mentions_any(error: &str, patterns: &[&str]) -> bool {
    patterns.iter().any(|pattern| mentions(error, pattern))
}

/// Whether `error` contains `pattern`. A number must stand alone, with no letter or digit
/// right before or after it, so status codes do not match inside request ids such as
/// `9d500a1b`.
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

    fn exact() -> RetryPolicy {
        RetryPolicy {
            jitter: 0.0,
            ..RetryPolicy::default()
        }
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
        let nominal = policy.delay(3);
        let mut waits = Vec::new();
        for _ in 0..64 {
            let wait = policy.delay_for(3, &failed("HTTP 503"));
            assert!(wait <= nominal && wait >= nominal.mul_f64(0.75), "{wait:?}");
            waits.push(wait);
        }
        assert!(waits.iter().any(|wait| *wait != nominal), "the waits vary");
        let mut hinted = failed("rate limited");
        hinted.provider_error = Some(ProviderError::Http {
            status: 429,
            code: None,
            retry_after_ms: Some(7000),
        });
        assert_eq!(policy.delay_for(1, &hinted), Duration::from_secs(7));
        assert_eq!(exact().delay_for(1, &failed("HTTP 503")), Duration::from_secs(2));
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
    fn structured_status_and_error_codes_override_text() {
        let mut message = failed("request_id: 401; billing service connection");
        message.provider_error = Some(ProviderError::Http {
            status: 503,
            code: None,
            retry_after_ms: None,
        });
        assert!(is_retryable(&message));
        message.provider_error = Some(ProviderError::Http {
            status: 400,
            code: None,
            retry_after_ms: None,
        });
        assert!(!is_retryable(&message));
        message.provider_error = Some(ProviderError::Http {
            status: 429,
            code: Some("insufficient_quota".into()),
            retry_after_ms: None,
        });
        assert!(!is_retryable(&message));
        message.provider_error = Some(ProviderError::ResourceLimit);
        assert!(!is_retryable(&message));
        message.provider_error = Some(ProviderError::ApiKey);
        assert!(!is_retryable(&message));
        assert!(!is_context_overflow(&message));
    }

    #[test]
    fn server_retry_hints_respect_the_configured_delay_cap() {
        let policy = exact();
        let mut message = failed("rate limited");
        message.provider_error = Some(ProviderError::Http {
            status: 429,
            code: None,
            retry_after_ms: Some(7000),
        });
        assert_eq!(policy.delay_for(1, &message), Duration::from_secs(7));
        assert_eq!(policy.delay_for(3, &message), Duration::from_secs(8));
        message.provider_error = Some(ProviderError::Http {
            status: 429,
            code: None,
            retry_after_ms: Some(120_000),
        });
        assert_eq!(policy.delay_for(1, &message), policy.max_delay);
    }

    fn http(status: u16, code: Option<&str>, error: &str) -> AssistantMessage {
        let mut message = failed(error);
        message.provider_error = Some(ProviderError::Http {
            status,
            code: code.map(str::to_string),
            retry_after_ms: None,
        });
        message
    }

    #[test]
    fn overflow_is_recognized_by_status_and_code() {
        assert!(is_context_overflow(&http(413, None, "HTTP 413: Payload Too Large")));
        assert!(is_context_overflow(&http(
            400,
            Some("context_length_exceeded"),
            "HTTP 400: bad request"
        )));
        let mut streamed = failed("the stream failed");
        streamed.provider_error = Some(ProviderError::Provider {
            code: Some("Model_Context_Window_Exceeded".into()),
        });
        assert!(is_context_overflow(&streamed));
    }

    #[test]
    fn overflow_is_recognized_by_generic_wording_without_a_code() {
        for error in [
            "HTTP 400: This model's maximum context length is 65536 tokens",
            "HTTP 400: Your input exceeds the context window of this model",
            "HTTP 400: prompt is too long: 213462 tokens > 200000 maximum",
            "HTTP 400: the request exceeds the available context size",
            "HTTP 413: request entity too large",
        ] {
            assert!(is_context_overflow(&failed(error)), "{error}");
        }
        assert!(is_context_overflow(&http(
            400,
            None,
            "HTTP 400: too many tokens in the prompt"
        )));
    }

    #[test]
    fn throttling_and_other_failures_are_not_overflow() {
        // Throttling may mention tokens, and a server error is not about the request's size.
        assert!(!is_context_overflow(&http(
            429,
            None,
            "HTTP 429: too many tokens, please wait"
        )));
        assert!(!is_context_overflow(&failed(
            "ThrottlingException: rate limit, too many tokens"
        )));
        assert!(!is_context_overflow(&http(
            503,
            None,
            "HTTP 503: maximum context of the queue"
        )));
        assert!(!is_context_overflow(&http(
            400,
            Some("invalid_request_error"),
            "HTTP 400: unknown model"
        )));
        let mut transport = failed("connection reset; context length unknown");
        transport.provider_error = Some(ProviderError::Transport);
        assert!(!is_context_overflow(&transport));
        assert!(!is_context_overflow(&AssistantMessage::failure(
            "p",
            "m",
            StopReason::Aborted,
            "context length exceeded"
        )));
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

    #[test]
    fn an_overflow_is_never_retried_whatever_else_the_error_says() {
        // Every overflow phrase and code is permanent, including those that only the
        // overflow tables knew: a transient phrase next to them does not make a retry.
        for error in [
            "HTTP 500: your input exceeds the context window of this model",
            "HTTP 503: prompt is too long for the model",
            "HTTP 502: request_too_large",
        ] {
            assert!(is_context_overflow(&failed(error)), "{error}");
            assert!(!is_retryable(&failed(error)), "{error}");
        }
        for code in ["model_context_window_exceeded", "request_too_large"] {
            let message = http(500, Some(code), "HTTP 500: rejected");
            assert!(is_context_overflow(&message), "{code}");
            assert!(!is_retryable(&message), "{code}");
        }
    }
}
