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
