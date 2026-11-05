//! Retry classification and backoff (pi: packages/ai/src/utils/retry.ts).

use std::time::Duration;

use super::types::{AssistantMessage, ProviderError, StopReason};

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

    /// Backoff including a server's numeric Retry-After hint, capped by max_delay.
    pub fn delay_for(&self, attempt: u32, message: &AssistantMessage) -> Duration {
        let hint = match &message.provider_error {
            Some(ProviderError::Http {
                retry_after_ms: Some(ms),
                ..
            }) => Duration::from_millis(*ms),
            _ => Duration::ZERO,
        };
        self.delay(attempt).max(hint).min(self.max_delay)
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

/// Whether a failed response is transient. Structured metadata takes precedence; messages
/// without it (from providers that report none, or read from older session files) use the
/// pi text fallback.
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
        Some(ProviderError::Protocol | ProviderError::ResourceLimit) => return false,
        Some(ProviderError::Provider { code: None }) | None => {}
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

/// Error codes that say the request was too long for the model. Codes, not vendor names: any
/// service may use them.
const OVERFLOW_CODES: &[&str] = &[
    "context_length_exceeded",
    "model_context_window_exceeded",
    "request_too_large",
];

/// Phrases that say the request was too long for the model, for failures without such a code.
/// Generic wording only, not one message per service (pi lists one per provider in
/// overflow.ts).
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

/// Throttling, which can also mention tokens ("too many tokens, please wait").
const THROTTLING_TEXT: &[&str] = &["429", "rate limit", "rate_limit", "throttl", "too many requests"];

/// Whether a failed response says the request was too long for the model's context: a
/// smaller context may fix it, repeating the request never does (pi: `isContextOverflow` in
/// packages/ai/src/utils/overflow.ts). An HTTP 413 or one of the overflow codes decides;
/// throttling, server, credential and transport failures never count; otherwise the error
/// text decides. Silent overflow, where a service truncates the input instead of failing, is
/// not detected.
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
            ProviderError::Transport | ProviderError::Timeout | ProviderError::Protocol | ProviderError::ResourceLimit,
        ) => {
            return false;
        }
        Some(ProviderError::Provider { .. }) | None => {}
    }
    let Some(error) = message.error_message.as_deref() else {
        return false;
    };
    let error = error.to_lowercase();
    if THROTTLING_TEXT.iter().any(|pattern| mentions(&error, pattern)) {
        return false;
    }
    error.starts_with("http 413") || OVERFLOW_TEXT.iter().any(|pattern| error.contains(pattern))
}

fn overflow_code(code: &str) -> bool {
    OVERFLOW_CODES.contains(&code.to_ascii_lowercase().as_str())
}

fn permanent_code(code: &str) -> bool {
    matches!(
        code.to_ascii_lowercase().as_str(),
        "insufficient_quota"
            | "billing_hard_limit_reached"
            | "invalid_api_key"
            | "context_length_exceeded"
            | "authentication_error"
            | "permission_error"
    )
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
    }

    #[test]
    fn server_retry_hints_respect_the_configured_delay_cap() {
        let policy = RetryPolicy::default();
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
}
