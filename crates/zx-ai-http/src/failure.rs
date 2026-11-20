//! Why a request did not end with a complete answer, and what a failed response says besides
//! its status: an excerpt of its body and the wait it asks for, read as milliseconds, seconds
//! or an HTTP date (pi: provider-retry.ts). What kind of failure a status, an error code or a
//! wording is, each provider says.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use zx_ai::{ProviderError, ProviderErrorKind};
use zx_util::text::clip;

/// Error bodies only add diagnostic detail; keep their memory use independent of the server.
const MAX_ERROR_BODY_BYTES: usize = 8 * 1024;

/// The most characters of a server's text that a failure message keeps.
const MAX_ERROR_EXCERPT_CHARS: usize = 2000;

/// Why a request did not end with a complete answer.
#[derive(Debug)]
pub enum Failure {
    /// The caller cancelled the request.
    Aborted,
    /// The consumer of the stream went away, so nothing is left to report to.
    Closed,
    /// The request failed, with the error for the message and its classification.
    Error { details: ProviderError, message: String },
}

impl Failure {
    /// A failed request: `details`, a whole [`ProviderError`] or only its kind, and the text
    /// for the message.
    pub fn error(details: impl Into<ProviderError>, message: impl Into<String>) -> Self {
        Self::Error {
            details: details.into(),
            message: message.into(),
        }
    }
}

/// The failure of `operation`, a request or a stream read, that `error` ended.
pub(crate) fn transport_failure(operation: &str, error: &reqwest::Error) -> Failure {
    let kind = if error.is_timeout() {
        ProviderErrorKind::Timeout
    } else {
        ProviderErrorKind::Transport
    };
    Failure::error(kind, format!("{operation}: {}", with_causes(error)))
}

/// Whether the server answered with a JSON document rather than an event stream: a gateway
/// can put an error object under a success status, and a server that ignores `stream: true`
/// sends the whole answer as JSON. Other content types, and one that is not text, are read as
/// a stream, so a server that labels its stream oddly keeps working.
pub fn answers_json(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("application/json"))
}

/// The start of a server's text, for a failure message.
pub fn excerpt(text: &str) -> String {
    clip(text.trim(), MAX_ERROR_EXCERPT_CHARS)
}

/// The start of a body read only to explain a failure, within `idle_timeout` and the
/// provider's `max_response_bytes`. A body that cannot be read in time loses only the
/// explanation, so it reads as empty.
pub async fn diagnostic_body(
    response: reqwest::Response,
    idle_timeout: Duration,
    max_response_bytes: usize,
) -> Vec<u8> {
    let read = read_error_body(response, MAX_ERROR_BODY_BYTES.min(max_response_bytes));
    match tokio::time::timeout(idle_timeout, read).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) | Err(_) => Vec::new(),
    }
}

async fn read_error_body(mut response: reqwest::Response, max_bytes: usize) -> Result<Vec<u8>, reqwest::Error> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        let keep = chunk.len().min(max_bytes.saturating_sub(bytes.len()));
        bytes.extend_from_slice(&chunk[..keep]);
        if bytes.len() == max_bytes {
            break;
        }
    }
    Ok(bytes)
}

/// The wait a server asks for, in milliseconds: `retry-after-ms` in milliseconds, else
/// `retry-after` in seconds, decimals allowed, or as an HTTP date (the IMF-fixdate of RFC
/// 9110, `Sun, 06 Nov 1994 08:49:37 GMT`), which counts from `now`; a date that has passed
/// is a wait of 0. Unreadable values are `None`, which leaves the wait to the local backoff.
pub fn retry_after_ms(headers: &reqwest::header::HeaderMap, now: SystemTime) -> Option<u64> {
    let text = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    if let Some(value) = text("retry-after-ms") {
        return value.parse::<f64>().ok().and_then(whole_milliseconds);
    }
    let value = text("retry-after")?;
    if let Ok(seconds) = value.parse::<f64>() {
        return whole_milliseconds(seconds * 1000.0);
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
/// read. Its numbers have fixed widths, the year four digits, which also keeps the
/// arithmetic below far from overflowing.
fn http_date_epoch_secs(text: &str) -> Option<u64> {
    let mut parts = text.split_ascii_whitespace();
    let weekday = parts.next()?;
    let day = fixed_digits(parts.next()?, 2)?;
    let month = parts.next()?;
    let year = fixed_digits(parts.next()?, 4)?;
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
    let mut clock = time.split(':').map(|field| fixed_digits(field, 2));
    let (hour, minute, second) = (clock.next()??, clock.next()??, clock.next()??);
    if clock.next().is_some()
        || !(1..=31).contains(&day)
        || !(0..24).contains(&hour)
        || !(0..60).contains(&minute)
        || !(0..61).contains(&second)
    {
        return None;
    }
    // Days since 1970-01-01 of a proleptic Gregorian date, in 400-year cycles that start on
    // March 1 (Howard Hinnant's days_from_civil; zx-builtin's calendar.rs has the inverse).
    let (year_from_march, month_from_march) = if month <= 2 {
        (year - 1, month + 9)
    } else {
        (year, month - 3)
    };
    let cycle = year_from_march.div_euclid(400);
    let year_of_cycle = year_from_march - cycle * 400;
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_cycle = year_of_cycle * 365 + year_of_cycle / 4 - year_of_cycle / 100 + day_of_year;
    let days = cycle * 146_097 + day_of_cycle - 719_468;
    u64::try_from(days * 86_400 + hour * 3_600 + minute * 60 + second).ok()
}

/// The number that `text` writes in exactly `width` ASCII digits.
fn fixed_digits(text: &str, width: usize) -> Option<i64> {
    if text.len() != width {
        return None;
    }
    text.bytes().try_fold(0, |value: i64, byte| {
        byte.is_ascii_digit().then(|| value * 10 + i64::from(byte - b'0'))
    })
}

/// `error` followed by its causes. A library's own message often omits the cause, such as
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

#[cfg(test)]
mod tests {
    use std::fmt;

    use super::*;

    #[derive(Debug)]
    struct Layer(&'static str, Option<Box<Layer>>);

    impl fmt::Display for Layer {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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
    fn the_causes_follow_the_error_and_a_repeated_one_is_left_out() {
        let error = Layer(
            "request failed: connection refused",
            Some(Box::new(Layer(
                "connection refused",
                Some(Box::new(Layer("os error 61", None))),
            ))),
        );
        assert_eq!(with_causes(&error), "request failed: connection refused: os error 61");
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

    #[test]
    fn a_date_whose_fields_are_not_of_their_fixed_widths_is_unreadable() {
        for bad in [
            // Far enough in the future that its seconds do not fit in an i64.
            "Sun, 06 Nov 99999999999999 08:49:37 GMT",
            "Sun, 06 Nov 994 08:49:37 GMT",
            "Sun, 06 Nov +994 08:49:37 GMT",
            "Sun, 6 Nov 1994 08:49:37 GMT",
            "Sun, 06 Nov 1994 8:49:37 GMT",
            "Sun, 06 Nov 1994 08:49:037 GMT",
        ] {
            assert_eq!(http_date_epoch_secs(bad), None, "{bad}");
        }
    }
}
