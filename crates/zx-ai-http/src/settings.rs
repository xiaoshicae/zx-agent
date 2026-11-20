//! The checks of the settings every HTTP provider has: where requests go, the headers they
//! carry, and the limits of their responses. Errors name the fields as the providers' settings
//! do.

use std::time::Duration;

/// Refuse an address requests cannot go to: one that is not an http or https URL, and one
/// with a query or a fragment, which the path of a request could not follow. The address may
/// hold a credential, so no error repeats it.
pub fn check_base_url(base_url: &str) -> anyhow::Result<()> {
    let url = reqwest::Url::parse(base_url).map_err(|error| anyhow::anyhow!("base_url is not a URL: {error}"))?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https"),
        "base_url must be an http or https URL"
    );
    anyhow::ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "base_url must end with its path, without a query or a fragment: the path of a request follows it"
    );
    Ok(())
}

/// Refuse headers the client cannot send as written, and `key_header`, which the API key
/// sets.
pub fn check_headers(headers: &[(String, String)], key_header: &str) -> anyhow::Result<()> {
    for (name, value) in headers {
        anyhow::ensure!(
            reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_ok(),
            "invalid request header name {name:?}"
        );
        anyhow::ensure!(
            reqwest::header::HeaderValue::from_str(value).is_ok(),
            "invalid value for request header {name:?}"
        );
        anyhow::ensure!(
            !name.eq_ignore_ascii_case(key_header),
            "the {key_header} header comes from the API key; it cannot be set as a request header"
        );
    }
    Ok(())
}

/// Refuse limits of zero: no byte of a response would fit, and a zero `idle_timeout` would
/// time out every request at once rather than disable the limit.
pub fn check_limits(
    idle_timeout: Duration,
    max_response_bytes: usize,
    max_sse_event_bytes: usize,
) -> anyhow::Result<()> {
    anyhow::ensure!(max_response_bytes > 0, "max_response_bytes must be greater than zero");
    anyhow::ensure!(max_sse_event_bytes > 0, "max_sse_event_bytes must be greater than zero");
    anyhow::ensure!(!idle_timeout.is_zero(), "idle_timeout must be greater than zero");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_is_an_http_url_that_a_path_can_follow() {
        for (base_url, expected) in [
            ("gateway.example.com/v1", "is not a URL"),
            ("localhost:8000/v1", "http or https"),
            ("ftp://gateway.example.com/v1", "http or https"),
            ("https://gateway.example.com/v1?version=1", "query"),
            ("https://gateway.example.com/v1#part", "fragment"),
        ] {
            let error = check_base_url(base_url).unwrap_err().to_string();
            assert!(error.contains(expected), "{base_url}: {error}");
            // The address may hold a credential, so the error does not repeat it.
            assert!(!error.contains("gateway"), "{error}");
        }
        for base_url in [
            "http://127.0.0.1:11434/v1",
            "http://localhost:8000",
            "https://api.openai.com/v1/",
        ] {
            assert!(check_base_url(base_url).is_ok(), "{base_url}");
        }
    }

    #[test]
    fn limits_of_zero_are_refused() {
        let second = Duration::from_secs(1);
        for (limits, field) in [
            (check_limits(Duration::ZERO, 1, 1), "idle_timeout"),
            (check_limits(second, 0, 1), "max_response_bytes"),
            (check_limits(second, 1, 0), "max_sse_event_bytes"),
        ] {
            let error = limits.unwrap_err().to_string();
            assert!(error.contains(&format!("{field} must be greater than zero")), "{error}");
        }
        assert!(check_limits(second, 1, 1).is_ok());
    }

    #[test]
    fn headers_the_client_cannot_send_are_refused() {
        let check = |name: &str, value: &str| check_headers(&[(name.into(), value.into())], "x-api-key");
        assert!(check("X-Route", "blue").is_ok());
        assert!(check("x-tenant", "a b").is_ok());
        assert!(
            check("bad name", "x")
                .unwrap_err()
                .to_string()
                .contains("invalid request header name")
        );
        assert!(
            check("x-a", "line\nbreak")
                .unwrap_err()
                .to_string()
                .contains("invalid value")
        );
    }

    #[test]
    fn the_header_the_api_key_sets_is_refused_in_any_case() {
        let error = check_headers(&[("X-Api-Key".into(), "k".into())], "x-api-key")
            .unwrap_err()
            .to_string();
        assert!(error.contains("comes from the API key"), "{error}");
    }
}
