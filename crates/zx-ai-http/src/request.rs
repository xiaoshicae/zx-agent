//! Posting a request: the API key, asked for before each request, and the wait for the
//! response headers, each within the provider's idle timeout.

use std::time::Duration;

use zx_ai::ProviderErrorKind;
use zx_ai::provider::ApiKey;

use crate::failure::{Failure, transport_failure};

/// The key `api_key` gives for this request, asked for on every request so that a key that
/// expired since the last one is replaced; `None` when there is none to send. Fails when its
/// source does not answer within `idle_timeout` or cannot give one: the request is not sent.
pub async fn api_key(api_key: &ApiKey, idle_timeout: Duration) -> Result<Option<String>, Failure> {
    tokio::time::timeout(idle_timeout, api_key.resolve())
        .await
        .map_err(|_| {
            Failure::error(
                ProviderErrorKind::ApiKey,
                format!("the API key source did not answer within {idle_timeout:?}"),
            )
        })?
        .map_err(|error| Failure::error(ProviderErrorKind::ApiKey, format!("cannot get an API key: {error}")))
}

/// Send `request` and wait for the response headers within `idle_timeout`.
pub async fn send(request: reqwest::RequestBuilder, idle_timeout: Duration) -> Result<reqwest::Response, Failure> {
    tokio::time::timeout(idle_timeout, request.send())
        .await
        .map_err(|_| {
            Failure::error(
                ProviderErrorKind::Timeout,
                format!("request timed out after {idle_timeout:?} waiting for response headers"),
            )
        })?
        .map_err(|error| transport_failure("request failed", &error))
}
