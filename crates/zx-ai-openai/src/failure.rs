//! How a request fails at an OpenAI-compatible server: the failures that its error statuses,
//! bodies and chunks report. What every HTTP provider shares, such as the transport's failures
//! and the Retry-After hint, is in `zx_ai_http::failure`.
//!
//! - `kind`: what kind of failure a server's status, error code or wording reports

pub(crate) mod kind;

use std::time::SystemTime;

use serde_json::Value;
use zx_ai::{ProviderError, ProviderErrorKind};
use zx_ai_http::Failure;
use zx_ai_http::failure::{diagnostic_body, excerpt, retry_after_ms};

use crate::config::OpenAiConfig;

/// The failure of a response with an error status, of the kind that the status, the body's
/// error code and its wording report ([`kind::of_status`]), with the server's Retry-After
/// hint. A hint the client cannot read is left to the local backoff, and a body that cannot
/// be read only loses its excerpt; the status still decides.
pub(crate) async fn http_failure(response: reqwest::Response, config: &OpenAiConfig) -> Failure {
    let status = response.status().as_u16();
    let retry_after_ms = retry_after_ms(response.headers(), SystemTime::now());
    let bytes = diagnostic_body(response, config.idle_timeout, config.max_response_bytes).await;
    let code = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|body| body.get("error").and_then(error_code));
    let text = excerpt(&String::from_utf8_lossy(&bytes));
    let details = ProviderError {
        kind: kind::of_status(status, code.as_deref(), &text),
        status: Some(status),
        code,
        retry_after_ms,
    };
    Failure::error(details, format!("HTTP {status}: {text}"))
}

/// What a JSON body under a success status says: its error object, or else that the server
/// did not stream. A body that cannot be read only loses its excerpt.
pub(crate) async fn json_body_failure(response: reqwest::Response, config: &OpenAiConfig) -> Failure {
    let bytes = diagnostic_body(response, config.idle_timeout, config.max_response_bytes).await;
    let body = serde_json::from_slice::<Value>(&bytes).ok();
    if let Some(error) = body.as_ref().and_then(error_object) {
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

/// The error object of a body or chunk, when it has one.
pub(crate) fn error_object(body: &Value) -> Option<&Value> {
    body.get("error").filter(|error| !error.is_null())
}

fn error_code(error: &Value) -> Option<String> {
    error
        .get("code")
        .and_then(Value::as_str)
        .or_else(|| error.get("type").and_then(Value::as_str))
        .map(str::to_string)
}

/// The failure that an error object, in a body or in a stream chunk, reports, of the kind
/// that its code or its wording says ([`kind::of_error`]).
pub(crate) fn provider_failure(error: &Value) -> Failure {
    let text = error
        .get("message")
        .and_then(Value::as_str)
        .map_or_else(|| error.to_string(), str::to_string);
    let text = excerpt(&text);
    let code = error_code(error);
    let details = ProviderError {
        kind: kind::of_error(code.as_deref(), &text),
        status: None,
        code,
        retry_after_ms: None,
    };
    Failure::error(details, format!("provider error: {text}"))
}
