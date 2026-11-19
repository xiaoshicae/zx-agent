//! How a request fails at the Messages API: the failures its error statuses, bodies and stream
//! events report, and what kind each is ([`ProviderErrorKind`]) by the statuses, error types
//! and wording of the API. The kernel acts on the kind alone, so the API's error types are
//! known here and nowhere else; what every HTTP provider shares, such as the transport's
//! failures and the Retry-After hint, is in `zx_ai_http::failure`.

use std::time::SystemTime;

use serde_json::Value;
use zx_ai::{ProviderError, ProviderErrorKind};
use zx_ai_http::Failure;
use zx_ai_http::failure::{diagnostic_body, excerpt, retry_after_ms};

use crate::config::AnthropicConfig;

/// What the API says of a thinking block whose signature binds it to the conversation as it
/// was, before its beginning changed, as a compaction or a changed system prompt changes it.
/// Sending it again never succeeds; the request does without it.
const BOUND_ELSEWHERE: &str = "bound to a different conversation";

/// Wording of a request that does not fit the model's context window, for an invalid request
/// whose error type does not say so.
const OVERFLOW_TEXT: &[&str] = &[
    "prompt is too long",
    "context limit",
    "context window",
    "too many tokens",
];

/// For `failure`, a request the API refused because a thinking block is bound to a different
/// conversation: the index of the message it named, from which on the thinking goes, or the
/// first message when it named none.
pub(crate) fn thinking_bound_elsewhere(failure: &Failure) -> Option<usize> {
    let Failure::Error { message, .. } = failure else {
        return None;
    };
    if !message.contains(BOUND_ELSEWHERE) {
        return None;
    }
    let named = message
        .split_once("messages.")
        .and_then(|(_, rest)| rest.split('.').next())
        .and_then(|index| index.parse().ok());
    Some(named.unwrap_or(0))
}

/// The failure of a response with an error status, of the kind its status and error type say
/// ([`of_status`]), with the server's Retry-After hint. A body that cannot be read only loses
/// its text; the status still decides.
pub(crate) async fn http_failure(response: reqwest::Response, config: &AnthropicConfig) -> Failure {
    let status = response.status().as_u16();
    let retry_after_ms = retry_after_ms(response.headers(), SystemTime::now());
    let bytes = diagnostic_body(response, config.idle_timeout, config.max_response_bytes).await;
    let body = serde_json::from_slice::<Value>(&bytes).ok();
    let error = body.as_ref().and_then(|body| body.get("error"));
    let code = error.and_then(error_type);
    let text = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map_or_else(|| String::from_utf8_lossy(&bytes).into_owned(), str::to_string);
    let text = excerpt(&text);
    let details = ProviderError {
        kind: of_status(status, code.as_deref(), &text),
        status: Some(status),
        code,
        retry_after_ms,
    };
    Failure::error(details, format!("HTTP {status}: {text}"))
}

/// What a JSON body under a success status says: its error, or else that the server did not
/// stream.
pub(crate) async fn json_body_failure(response: reqwest::Response, config: &AnthropicConfig) -> Failure {
    let bytes = diagnostic_body(response, config.idle_timeout, config.max_response_bytes).await;
    let body = serde_json::from_slice::<Value>(&bytes).ok();
    if let Some(error) = body
        .as_ref()
        .and_then(|body| body.get("error"))
        .filter(|error| error.is_object())
    {
        return provider_failure(error);
    }
    Failure::error(
        ProviderErrorKind::Protocol,
        format!(
            "the server answered with JSON instead of an event stream; it may not support streaming: {}",
            excerpt(&String::from_utf8_lossy(&bytes))
        ),
    )
}

/// The failure that an error, of an `error` event of the stream or of a JSON body, reports,
/// of the kind its type says ([`of_type`]).
pub(crate) fn provider_failure(error: &Value) -> Failure {
    let code = error_type(error);
    let text = error
        .get("message")
        .and_then(Value::as_str)
        .map_or_else(|| error.to_string(), str::to_string);
    let text = excerpt(&text);
    let details = ProviderError {
        kind: of_type(code.as_deref(), &text),
        status: None,
        code,
        retry_after_ms: None,
    };
    Failure::error(details, format!("provider error: {text}"))
}

fn error_type(error: &Value) -> Option<String> {
    error.get("type").and_then(Value::as_str).map(str::to_string)
}

/// The kind of failure a response's status says, with its error type and wording where the
/// status alone does not: 529, the API's overload, is one of the 5xx statuses.
pub(crate) fn of_status(status: u16, code: Option<&str>, text: &str) -> ProviderErrorKind {
    match status {
        429 => ProviderErrorKind::RateLimited,
        408 => ProviderErrorKind::Timeout,
        401..=403 => ProviderErrorKind::AccessDenied,
        413 => ProviderErrorKind::ContextOverflow,
        500..=599 => ProviderErrorKind::Unavailable,
        _ => of_type(code, text),
    }
}

/// The kind an error type of the API says, and the wording of an invalid request that is too
/// long for the model.
pub(crate) fn of_type(code: Option<&str>, text: &str) -> ProviderErrorKind {
    match code {
        Some("rate_limit_error") => ProviderErrorKind::RateLimited,
        Some("overloaded_error" | "api_error") => ProviderErrorKind::Unavailable,
        Some("timeout_error") => ProviderErrorKind::Timeout,
        Some("authentication_error" | "permission_error" | "billing_error") => ProviderErrorKind::AccessDenied,
        Some("request_too_large") => ProviderErrorKind::ContextOverflow,
        _ if says_overflow(text) => ProviderErrorKind::ContextOverflow,
        _ => ProviderErrorKind::Other,
    }
}

fn says_overflow(text: &str) -> bool {
    let text = text.to_lowercase();
    OVERFLOW_TEXT.iter().any(|phrase| text.contains(phrase))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn statuses_and_error_types_say_what_kind_of_failure_it_is() {
        use ProviderErrorKind::*;
        for (status, code, kind) in [
            (429, Some("rate_limit_error"), RateLimited),
            (529, Some("overloaded_error"), Unavailable),
            (500, Some("api_error"), Unavailable),
            (401, Some("authentication_error"), AccessDenied),
            (402, Some("billing_error"), AccessDenied),
            (403, Some("permission_error"), AccessDenied),
            (413, Some("request_too_large"), ContextOverflow),
            (404, Some("not_found_error"), Other),
            (400, Some("invalid_request_error"), Other),
        ] {
            assert_eq!(of_status(status, code, "x"), kind, "{status}");
        }
        assert_eq!(
            of_status(
                400,
                Some("invalid_request_error"),
                "prompt is too long: 215000 tokens > 200000 maximum"
            ),
            ContextOverflow
        );
        assert_eq!(of_type(Some("overloaded_error"), "Overloaded"), Unavailable);
        assert_eq!(of_type(None, "something else"), Other);
    }

    #[test]
    fn an_error_event_is_a_provider_failure_of_its_type() {
        let Failure::Error { details, message } =
            provider_failure(&json!({"type": "overloaded_error", "message": "Overloaded"}))
        else {
            panic!("expected an error");
        };
        assert_eq!(details.kind, ProviderErrorKind::Unavailable);
        assert_eq!(details.code.as_deref(), Some("overloaded_error"));
        assert_eq!(message, "provider error: Overloaded");
    }

    #[test]
    fn a_thinking_block_bound_elsewhere_names_where_the_thinking_goes_from() {
        let refused = |text: &str| {
            thinking_bound_elsewhere(&Failure::error(ProviderErrorKind::Other, format!("HTTP 400: {text}")))
        };
        assert_eq!(
            refused(
                "messages.4.content.0: Invalid `signature` in `thinking` block. The block is bound to a different conversation. Remove the block."
            ),
            Some(4)
        );
        assert_eq!(
            refused("Invalid `signature` in `thinking` block. The block is bound to a different conversation."),
            Some(0)
        );
        // A tampered signature is refused whatever is sent: no retry.
        assert_eq!(
            refused("messages.4.content.0: Invalid `signature` in `thinking` block."),
            None
        );
    }
}
