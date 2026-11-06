//! How a request fails: the failures of HTTP responses, bodies and chunks, with the server's
//! Retry-After hint read as milliseconds, seconds or an HTTP date (pi: provider-retry.ts).

use std::time::{SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use serde_json::Value;
use zx_ai::ProviderError;
use zx_ai::text::clip;

use crate::config::OpenAiConfig;

/// Error bodies only add diagnostic detail; keep their memory use independent of the server.
const MAX_ERROR_BODY_BYTES: usize = 8 * 1024;

/// The most characters of a server's text that a failure message keeps.
const MAX_ERROR_EXCERPT_CHARS: usize = 2000;

/// Why a request did not end with a complete answer.
pub(crate) enum Failure {
    /// The caller cancelled the request.
    Aborted,
    /// The consumer of the stream went away, so nothing is left to report to.
    Closed,
    /// The request failed, with the error for the message and its classification.
    Error { details: ProviderError, message: String },
}

impl Failure {
    pub(crate) fn error(details: ProviderError, message: impl Into<String>) -> Self {
        Self::Error {
            details,
            message: message.into(),
        }
    }
}

/// The failure of a response with an error status: the body's error code and the server's
/// Retry-After hint go with it. A hint the client cannot read is left to the local backoff,
/// and a body that cannot be read only loses its excerpt; the status is authoritative.
pub(crate) async fn http_failure(response: reqwest::Response, config: &OpenAiConfig) -> Failure {
    let status = response.status().as_u16();
    let retry_after_ms = retry_after_ms(response.headers(), SystemTime::now());
    let bytes = diagnostic_body(response, config).await;
    let code = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|body| body.get("error").and_then(error_code));
    Failure::error(
        ProviderError::Http {
            status,
            code,
            retry_after_ms,
        },
        format!("HTTP {status}: {}", excerpt(&String::from_utf8_lossy(&bytes))),
    )
}

/// Whether the server answered with a JSON document rather than an event stream: a gateway
/// can put an error object under a success status, and a server that ignores `stream: true`
/// sends the whole answer as JSON. Other content types are read as a stream, so a server that
/// labels its stream oddly keeps working.
pub(crate) fn answers_json(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("application/json"))
}

/// What a JSON body under a success status says: its error object, or else that the server
/// did not stream. A body that cannot be read only loses its excerpt.
pub(crate) async fn json_body_failure(response: reqwest::Response, config: &OpenAiConfig) -> Failure {
    let bytes = diagnostic_body(response, config).await;
    let body = serde_json::from_slice::<Value>(&bytes).ok();
    if let Some(error) = body.as_ref().and_then(error_object) {
        return provider_failure(error);
    }
    Failure::error(
        ProviderError::Protocol,
        format!(
            "the server answered with JSON instead of an event stream; it may not support streaming: {}",
            excerpt(&String::from_utf8_lossy(&bytes))
        ),
    )
}

/// The start of a server's text, for a failure message.
fn excerpt(text: &str) -> String {
    clip(text.trim(), MAX_ERROR_EXCERPT_CHARS)
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

/// The failure that an error object, in a body or in a stream chunk, reports.
pub(crate) fn provider_failure(error: &Value) -> Failure {
    let text = error
        .get("message")
        .and_then(Value::as_str)
        .map_or_else(|| error.to_string(), str::to_string);
    Failure::error(
        ProviderError::Provider {
            code: error_code(error),
        },
        format!("provider error: {}", excerpt(&text)),
    )
}

/// The failure of `operation`, a request or a stream read, that `error` ended.
pub(crate) fn transport_failure(operation: &str, error: &reqwest::Error) -> Failure {
    let details = if error.is_timeout() {
        ProviderError::Timeout
    } else {
        ProviderError::Transport
    };
    Failure::error(details, format!("{operation}: {}", with_causes(error)))
}

/// The start of a body read only to explain a failure, within the idle timeout. A body that
/// cannot be read in time loses only the explanation, so it reads as empty.
async fn diagnostic_body(response: reqwest::Response, config: &OpenAiConfig) -> Vec<u8> {
    let read = read_error_body(response, MAX_ERROR_BODY_BYTES.min(config.max_response_bytes));
    match tokio::time::timeout(config.idle_timeout, read).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) | Err(_) => Vec::new(),
    }
}

async fn read_error_body(response: reqwest::Response, max_bytes: usize) -> Result<Vec<u8>, reqwest::Error> {
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let keep = chunk.len().min(max_bytes.saturating_sub(bytes.len()));
        bytes.extend_from_slice(&chunk[..keep]);
        if bytes.len() == max_bytes {
            break;
        }
    }
    Ok(bytes)
}

/// `error` followed by its causes. reqwest's own message omits the cause, such as
/// "Connection refused", which is what an unattended run needs for diagnosis. A cause whose
/// text is already included is skipped.
fn with_causes(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.contains(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

/// The wait a server asks for, in milliseconds: `retry-after-ms` in milliseconds, else
/// `retry-after` in seconds, decimals allowed, or as an HTTP date (the IMF-fixdate of RFC
/// 9110, `Sun, 06 Nov 1994 08:49:37 GMT`), which counts from `now`; a date that has passed
/// is a wait of 0. Unreadable values are `None`.
fn retry_after_ms(headers: &reqwest::header::HeaderMap, now: SystemTime) -> Option<u64> {
    let text = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    let whole = whole_milliseconds;
    if let Some(value) = text("retry-after-ms") {
        return value.parse::<f64>().ok().and_then(whole);
    }
    let value = text("retry-after")?;
    if let Ok(seconds) = value.parse::<f64>() {
        return whole(seconds * 1000.0);
    }
    let at = http_date_epoch_secs(value)?;
    let now = now.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(at.saturating_sub(now).saturating_mul(1000))
}

/// `value` as whole milliseconds, for a finite, non-negative value.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "guarded: finite and non-negative; a value beyond u64 saturates"
)]
fn whole_milliseconds(value: f64) -> Option<u64> {
    (value.is_finite() && value >= 0.0).then(|| value.round() as u64)
}

/// Seconds since the Unix epoch of an IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`), the
/// one date format servers must send (RFC 9110 §5.6.7); the two obsolete formats are not
/// read.
fn http_date_epoch_secs(text: &str) -> Option<u64> {
    let mut parts = text.split_ascii_whitespace();
    let weekday = parts.next()?;
    let day: i64 = parts.next()?.parse().ok()?;
    let month = parts.next()?;
    let year: i64 = parts.next()?.parse().ok()?;
    let time = parts.next()?;
    let zone = parts.next()?;
    if parts.next().is_some() || !weekday.ends_with(',') || zone != "GMT" {
        return None;
    }
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|name| *name == month)?;
    let month = i64::try_from(month).ok()? + 1;
    let mut clock = time.split(':').map(|field| field.parse::<i64>().ok());
    let (hour, minute, second) = (clock.next()??, clock.next()??, clock.next()??);
    if clock.next().is_some()
        || !(1..=31).contains(&day)
        || !(0..24).contains(&hour)
        || !(0..60).contains(&minute)
        || !(0..61).contains(&second)
    {
        return None;
    }
    // Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's days_from_civil).
    let (y, m) = if month <= 2 {
        (year - 1, month + 9)
    } else {
        (year, month - 3)
    };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hour * 3_600 + minute * 60 + second).ok()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[derive(Debug)]
    struct Layer(&'static str, Option<Box<Layer>>);

    impl std::fmt::Display for Layer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }

    impl std::error::Error for Layer {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.1
                .as_deref()
                .map(|cause| cause as &(dyn std::error::Error + 'static))
        }
    }

    #[test]
    fn error_text_includes_causes_once() {
        let refused = Layer("Connection refused", None);
        let connect = Layer("tcp connect error: Connection refused", Some(Box::new(refused)));
        let error = Layer("error sending request", Some(Box::new(connect)));
        assert_eq!(
            with_causes(&error),
            "error sending request: tcp connect error: Connection refused"
        );
    }

    #[test]
    fn retry_after_hints_are_read_as_milliseconds_seconds_or_a_date() {
        use reqwest::header::{HeaderMap, HeaderValue};
        let now = UNIX_EPOCH + Duration::from_secs(784_111_777);
        let headers = |name: &str, value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
            headers
        };
        assert_eq!(retry_after_ms(&headers("retry-after-ms", "1500.4"), now), Some(1500));
        assert_eq!(retry_after_ms(&headers("retry-after", "2.5"), now), Some(2500));
        assert_eq!(retry_after_ms(&headers("retry-after", "7"), now), Some(7000));
        assert_eq!(
            retry_after_ms(&headers("retry-after", "Sun, 06 Nov 1994 08:49:47 GMT"), now),
            Some(10_000)
        );
        assert_eq!(
            retry_after_ms(&headers("retry-after", "Sun, 06 Nov 1994 08:49:27 GMT"), now),
            Some(0),
            "a date that passed is a wait of 0"
        );
        assert_eq!(retry_after_ms(&headers("retry-after", "soon"), now), None);
        assert_eq!(retry_after_ms(&headers("retry-after", "-3"), now), None);
        assert_eq!(retry_after_ms(&HeaderMap::new(), now), None);
        // retry-after-ms wins over retry-after.
        let mut both = headers("retry-after-ms", "250");
        both.insert("retry-after", HeaderValue::from_static("9"));
        assert_eq!(retry_after_ms(&both, now), Some(250));
    }

    #[test]
    fn http_dates_parse_to_epoch_seconds() {
        assert_eq!(http_date_epoch_secs("Sun, 06 Nov 1994 08:49:37 GMT"), Some(784_111_777));
        assert_eq!(http_date_epoch_secs("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(
            http_date_epoch_secs("Tue, 29 Feb 2028 23:59:60 GMT"),
            Some(1_835_481_600)
        );
        for bad in [
            "Sun, 06 Nov 1994 08:49:37 PST",
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun Nov  6 08:49:37 1994",
            "Sun, 32 Nov 1994 08:49:37 GMT",
            "Sun, 06 Foo 1994 08:49:37 GMT",
            "Sun, 06 Nov 1994 24:00:00 GMT",
            "",
        ] {
            assert_eq!(http_date_epoch_secs(bad), None, "{bad}");
        }
    }
}
