//! What kind of failure ([`ProviderErrorKind`]) a server reports: the HTTP status and the
//! error codes of OpenAI-compatible services decide, and the wording of the error where they
//! do not (pi: packages/ai/src/utils/retry.ts and overflow.ts). The kernel acts on the kind
//! alone, so the codes and phrases of services are known here and nowhere else.

use zx_ai::ProviderErrorKind;

/// Phrases that say the request was too long for the model, for failures without a code that
/// says so. Generic wording only, not one message per service.
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

/// Wording of a refusal that someone has to fix: credentials, permissions and quota.
const ACCESS_TEXT: &[&str] = &[
    "401",
    "403",
    "unauthorized",
    "invalid api key",
    "invalid_api_key",
    "insufficient_quota",
    "billing",
];

/// Wording of rate limiting, throttling included. It can mention tokens too ("too many
/// tokens, please wait"), so it is never taken for an overflow.
const RATE_LIMIT_TEXT: &[&str] = &["429", "rate limit", "rate_limit", "too many requests", "throttl"];

/// Wording of a service that says it timed out itself.
const TIMEOUT_TEXT: &[&str] = &["timed out"];

/// Wording of a service that fails, is overloaded or cannot be reached for now. A gateway's
/// timeout is the gateway's status, a 504, which is a failing service's.
const UNAVAILABLE_TEXT: &[&str] = &[
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
    "connection",
    "temporarily",
];

/// The kind of failure of a response with the error status `status`, the error code `code`
/// and the body `text`. A request too long for the model comes first: a 413, an overflow
/// code, or overflow wording under a status that does not say otherwise. Then a refusal of
/// credentials, permissions or quota, by its status or its code; otherwise the status decides.
pub(crate) fn of_status(status: u16, code: Option<&str>, text: &str) -> ProviderErrorKind {
    let by_code = code.and_then(of_code);
    // Refused credentials, a timeout, rate limiting and a server's failure are about something
    // other than the request's size, whatever their wording mentions.
    let status_decides = matches!(status, 401 | 403 | 408 | 429 | 500..=599);
    if status == 413
        || by_code == Some(ProviderErrorKind::ContextOverflow)
        || (!status_decides && says_overflow(&text.to_lowercase()))
    {
        ProviderErrorKind::ContextOverflow
    } else if matches!(status, 401 | 403) || by_code == Some(ProviderErrorKind::AccessDenied) {
        ProviderErrorKind::AccessDenied
    } else {
        match status {
            408 => ProviderErrorKind::Timeout,
            429 => ProviderErrorKind::RateLimited,
            500..=599 => ProviderErrorKind::Unavailable,
            _ => ProviderErrorKind::Other,
        }
    }
}

/// The kind of failure that an error object reports, in a body under a success status or in
/// the stream: what its `code` says when the code is a known one, otherwise what its wording
/// `text` says.
pub(crate) fn of_error(code: Option<&str>, text: &str) -> ProviderErrorKind {
    match code.and_then(of_code) {
        Some(kind) => kind,
        // A code that is not a known one says nothing either way.
        None => of_wording(&text.to_lowercase()),
    }
}

/// The kind of failure of a response that the service ended with `reason`, a finish reason
/// that is not an answer's. A reason that names an overflow is one, since some services end
/// a response to a request that does not fit that way (`model_context_window_exceeded`); any
/// other reason is a refusal, whatever it is called, so that none of them is retried.
pub(crate) fn of_finish_reason(reason: &str) -> ProviderErrorKind {
    if says_overflow(&reason.to_lowercase()) {
        ProviderErrorKind::ContextOverflow
    } else {
        ProviderErrorKind::Refused
    }
}

/// The kind that the error code `code` says, for the codes that say one. Codes, not vendor
/// names: any service may use them.
fn of_code(code: &str) -> Option<ProviderErrorKind> {
    let kind = match code.to_ascii_lowercase().as_str() {
        "context_length_exceeded" | "model_context_window_exceeded" | "request_too_large" => {
            ProviderErrorKind::ContextOverflow
        }
        "insufficient_quota"
        | "billing_hard_limit_reached"
        | "invalid_api_key"
        | "authentication_error"
        | "permission_error" => ProviderErrorKind::AccessDenied,
        "rate_limit_exceeded" | "rate_limit_error" => ProviderErrorKind::RateLimited,
        "server_error" | "internal_error" | "overloaded_error" | "server_busy" => ProviderErrorKind::Unavailable,
        "timeout" => ProviderErrorKind::Timeout,
        _ => return None,
    };
    Some(kind)
}

/// The kind that the lowercased wording `text` of an error says: an overflow first, then a
/// refusal of credentials, permissions or quota, rate limiting, a timeout and an unavailable
/// service.
fn of_wording(text: &str) -> ProviderErrorKind {
    if says_overflow(text) {
        ProviderErrorKind::ContextOverflow
    } else if mentions_any(text, ACCESS_TEXT) {
        ProviderErrorKind::AccessDenied
    } else if mentions_any(text, RATE_LIMIT_TEXT) {
        ProviderErrorKind::RateLimited
    } else if mentions_any(text, TIMEOUT_TEXT) {
        ProviderErrorKind::Timeout
    } else if mentions_any(text, UNAVAILABLE_TEXT) {
        ProviderErrorKind::Unavailable
    } else {
        ProviderErrorKind::Other
    }
}

/// Whether the lowercased `text` says the request was too long for the model: one of the
/// overflow phrases, and no wording of rate limiting.
fn says_overflow(text: &str) -> bool {
    !mentions_any(text, RATE_LIMIT_TEXT) && mentions_any(text, OVERFLOW_TEXT)
}

/// Whether the lowercased `text` mentions one of `patterns`, as [`mentions`] decides.
fn mentions_any(text: &str, patterns: &[&str]) -> bool {
    patterns.iter().any(|pattern| mentions(text, pattern))
}

/// Whether `text` contains `pattern`. A number must stand alone, with no letter or digit
/// right before or after it, so status codes do not match inside request ids such as
/// `9d500a1b`.
fn mentions(text: &str, pattern: &str) -> bool {
    if !pattern.bytes().all(|byte| byte.is_ascii_digit()) {
        return text.contains(pattern);
    }
    text.match_indices(pattern).any(|(start, _)| {
        let before = text[..start].chars().next_back();
        let after = text[start + pattern.len()..].chars().next();
        ![before, after].into_iter().flatten().any(|c| c.is_ascii_alphanumeric())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_codes_in_wording_match_only_as_whole_numbers() {
        // Request ids are hex strings, which can contain digits such as "500" or "401".
        assert_eq!(
            of_error(None, "HTTP 400: unknown model (request_id: 9d500a1b-0c2e)"),
            ProviderErrorKind::Other
        );
        assert_eq!(
            of_error(None, "HTTP 503: busy (request_id: 1401ab-77)"),
            ProviderErrorKind::Unavailable
        );
    }

    #[test]
    fn the_status_and_the_error_code_decide_over_the_wording() {
        let text = "request_id: 401; billing service connection";
        assert_eq!(of_status(503, None, text), ProviderErrorKind::Unavailable);
        assert_eq!(of_status(400, None, text), ProviderErrorKind::Other);
        assert_eq!(
            of_status(429, Some("insufficient_quota"), text),
            ProviderErrorKind::AccessDenied
        );
        assert_eq!(of_status(429, None, "slow down"), ProviderErrorKind::RateLimited);
        assert_eq!(of_status(408, None, "slow down"), ProviderErrorKind::Timeout);
        assert_eq!(of_status(401, None, "bad key"), ProviderErrorKind::AccessDenied);
    }

    #[test]
    fn an_unknown_error_code_leaves_the_kind_to_the_wording() {
        assert_eq!(
            of_error(Some("server_overloaded"), "overloaded"),
            ProviderErrorKind::Unavailable
        );
        assert_eq!(
            of_error(Some("server_overloaded"), "unknown model"),
            ProviderErrorKind::Other
        );
        assert_eq!(
            of_error(Some("server_busy"), "unknown model"),
            ProviderErrorKind::Unavailable,
            "a known code decides"
        );
        assert_eq!(
            of_error(Some("invalid_api_key"), "overloaded"),
            ProviderErrorKind::AccessDenied,
            "so does one that someone has to fix"
        );
        assert_eq!(
            of_error(Some("context_length_exceeded"), "overloaded"),
            ProviderErrorKind::ContextOverflow
        );
    }

    #[test]
    fn overflow_is_recognized_by_status_and_code() {
        assert_eq!(
            of_status(413, None, "Payload Too Large"),
            ProviderErrorKind::ContextOverflow
        );
        assert_eq!(
            of_status(400, Some("context_length_exceeded"), "bad request"),
            ProviderErrorKind::ContextOverflow
        );
        assert_eq!(
            of_error(Some("Model_Context_Window_Exceeded"), "the stream failed"),
            ProviderErrorKind::ContextOverflow
        );
    }

    #[test]
    fn overflow_is_recognized_by_generic_wording_without_a_code() {
        for text in [
            "This model's maximum context length is 65536 tokens",
            "Your input exceeds the context window of this model",
            "prompt is too long: 213462 tokens > 200000 maximum",
            "the request exceeds the available context size",
        ] {
            assert_eq!(of_status(400, None, text), ProviderErrorKind::ContextOverflow, "{text}");
            assert_eq!(of_error(None, text), ProviderErrorKind::ContextOverflow, "{text}");
        }
        assert_eq!(
            of_status(413, None, "request entity too large"),
            ProviderErrorKind::ContextOverflow
        );
        assert_eq!(
            of_status(400, None, "too many tokens in the prompt"),
            ProviderErrorKind::ContextOverflow
        );
    }

    #[test]
    fn throttling_and_other_failures_are_not_overflow() {
        // Throttling may mention tokens, and a server error is not about the request's size.
        assert_eq!(
            of_status(429, None, "too many tokens, please wait"),
            ProviderErrorKind::RateLimited
        );
        for text in [
            "ThrottlingException: rate limit, too many tokens",
            "ThrottlingException: too many tokens",
        ] {
            assert_eq!(of_error(None, text), ProviderErrorKind::RateLimited, "{text}");
        }
        assert_eq!(
            of_status(503, None, "maximum context of the queue"),
            ProviderErrorKind::Unavailable
        );
        assert_eq!(
            of_status(400, Some("invalid_request_error"), "unknown model"),
            ProviderErrorKind::Other
        );
    }

    #[test]
    fn classifies_errors_by_their_wording() {
        for (text, kind) in [
            (
                "The service is unavailable: service unavailable",
                ProviderErrorKind::Unavailable,
            ),
            ("overloaded_error", ProviderErrorKind::Unavailable),
            ("The upstream request timed out", ProviderErrorKind::Timeout),
            ("Too many requests, slow down", ProviderErrorKind::RateLimited),
            ("Invalid API key provided", ProviderErrorKind::AccessDenied),
            (
                "You exceeded your current quota: insufficient_quota",
                ProviderErrorKind::AccessDenied,
            ),
            ("context_length_exceeded", ProviderErrorKind::ContextOverflow),
        ] {
            assert_eq!(of_error(None, text), kind, "{text}");
        }
    }

    #[test]
    fn a_failed_request_is_not_taken_for_an_unavailable_service() {
        // The words this client used for its own failures say nothing about a server's error.
        assert_eq!(
            of_error(None, "Request failed validation: unknown parameter"),
            ProviderErrorKind::Other
        );
    }

    #[test]
    fn an_overflow_is_never_taken_for_a_transient_failure_whatever_else_the_error_says() {
        // Every overflow phrase and code is an overflow, including those that only the
        // overflow tables know: a transient phrase next to them does not make it transient.
        for text in [
            "Internal server error: your input exceeds the context window of this model",
            "Service unavailable: prompt is too long for the model",
            "Bad gateway: request_too_large",
        ] {
            assert_eq!(of_error(None, text), ProviderErrorKind::ContextOverflow, "{text}");
        }
        for code in ["model_context_window_exceeded", "request_too_large"] {
            assert_eq!(
                of_status(500, Some(code), "rejected"),
                ProviderErrorKind::ContextOverflow,
                "{code}"
            );
        }
    }

    #[test]
    fn a_finish_reason_is_a_refusal_unless_it_names_an_overflow() {
        for reason in [
            "content_filter",
            "network_error",
            "timeout",
            "rate_limit_exceeded",
            "server_error",
        ] {
            assert_eq!(of_finish_reason(reason), ProviderErrorKind::Refused, "{reason}");
        }
        assert_eq!(
            of_finish_reason("model_context_window_exceeded"),
            ProviderErrorKind::ContextOverflow
        );
    }
}
