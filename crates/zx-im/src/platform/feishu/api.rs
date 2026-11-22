//! The open API: the tenant token, calls with frequency limits waited out, and why a call
//! did not succeed.

use std::time::Duration;

use reqwest::{Method, StatusCode, Url};
use serde_json::{Value, json};
use tokio::time::Instant;

use super::FeishuChannel;
use crate::ImError;
use crate::platform::{RATE_LIMIT_RETRIES, body_within, content_type_is, rate_limit_wait};

/// A token is renewed when less than this remains; the Go SDK renews three minutes early.
const TOKEN_MARGIN: Duration = Duration::from_secs(5 * 60);
/// Longest token lifetime accepted; tenant tokens last at most two hours.
const MAX_TOKEN_LIFETIME: Duration = Duration::from_secs(24 * 3600);
/// Codes that mean a frequency limit: 99991400 from the gateway (frequency-control guide)
/// and 230020 from the message APIs.
const RATE_LIMIT_CODES: [i64; 2] = [99_991_400, 230_020];
/// Header with the seconds to wait after a frequency limit.
const RATE_LIMIT_RESET: &str = "x-ogw-ratelimit-reset";

/// Why an API call failed.
pub(super) enum CallError {
    /// Feishu answered with a code other than 0: it refused the call, as this says for the
    /// operator, the code included.
    Refused(String),
    /// No answer came that says how the call went, or no token could be had: the secret
    /// was refused, or the token endpoint failed.
    Failed(ImError),
}

impl From<CallError> for ImError {
    fn from(error: CallError) -> Self {
        match error {
            CallError::Refused(message) => ImError::Transport(message),
            CallError::Failed(error) => error,
        }
    }
}

impl FeishuChannel {
    /// The tenant token, renewed when it is about to expire. A refused secret is a
    /// configuration error.
    async fn token(&self) -> Result<String, ImError> {
        let mut cached = self.token.lock().await;
        if let Some((token, until)) = cached.as_ref()
            && Instant::now() + TOKEN_MARGIN < *until
        {
            return Ok(token.clone());
        }
        let body = json!({
            "grant_type": "client_credentials",
            "client_id": self.config.app_id,
            "client_secret": self.config.app_secret,
        });
        let response = self
            .http
            .post(&self.config.token_url)
            .json(&body)
            .send()
            .await
            .map_err(|error| ImError::Transport(format!("feishu token: {error}")))?;
        let status = response.status();
        let answer: Value = response
            .json()
            .await
            .map_err(|error| ImError::Transport(format!("feishu token: HTTP {status}: {error}")))?;
        let (Some(0), Some(token), Some(secs)) = (
            answer["code"].as_i64(),
            answer["access_token"].as_str(),
            answer["expires_in"].as_u64(),
        ) else {
            let detail = answer["error_description"]
                .as_str()
                .or_else(|| answer["msg"].as_str())
                .unwrap_or("no description");
            let message = format!("feishu token: HTTP {status}, code {}: {detail}", answer["code"]);
            return Err(if status.is_client_error() {
                ImError::Config(message)
            } else {
                ImError::Transport(message)
            });
        };
        let lifetime = Duration::from_secs(secs).min(MAX_TOKEN_LIFETIME);
        *cached = Some((token.to_string(), Instant::now() + lifetime));
        Ok(token.to_string())
    }

    /// The file a resource API answers `path` with, at most `max_bytes`. An answer in JSON,
    /// or one with a status other than success, is a refusal, its code included.
    pub(super) async fn resource(&self, path: &str, max_bytes: usize) -> Result<Vec<u8>, ImError> {
        let url = format!("{}{path}", self.config.api_base.trim_end_matches('/'));
        let token = self.token().await?;
        let response = self
            .http
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|error| ImError::Transport(format!("feishu {path}: {error}")))?;
        let status = response.status();
        if !status.is_success() || content_type_is(&response, "application/json") {
            // A body that is not JSON leaves no code; the status then explains the failure.
            let answer: Option<Value> = response.json().await.ok();
            let code = answer.as_ref().map_or(Value::Null, |answer| answer["code"].clone());
            let detail = answer
                .as_ref()
                .and_then(|answer| answer["msg"].as_str())
                .unwrap_or("no message");
            return Err(ImError::Transport(format!(
                "feishu {path}: HTTP {status}, code {code}: {detail}"
            )));
        }
        body_within(response, max_bytes)
            .await
            .map_err(|error| ImError::Transport(format!("feishu {path}: {error}")))
    }

    /// Call an open API method, with `args` added to the query and a JSON `body` if any. A
    /// frequency limit is waited out and retried.
    pub(super) async fn api(
        &self,
        method: Method,
        path: &str,
        args: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<Value, CallError> {
        let base = self.config.api_base.trim_end_matches('/');
        let mut url = Url::parse(&format!("{base}{path}")).map_err(|error| {
            CallError::Failed(ImError::Config(format!(
                "feishu {path}: invalid API base {base}: {error}"
            )))
        })?;
        if !args.is_empty() {
            url.query_pairs_mut().extend_pairs(args);
        }
        let mut retries = 0;
        loop {
            let token = self.token().await.map_err(CallError::Failed)?;
            let request = self.http.request(method.clone(), url.clone()).bearer_auth(&token);
            let request = match body {
                Some(body) => request.json(body),
                None => request,
            };
            let response = request
                .send()
                .await
                .map_err(|error| CallError::Failed(ImError::Transport(format!("feishu {path}: {error}"))))?;
            let status = response.status();
            let wait = rate_limit_wait(response.headers(), RATE_LIMIT_RESET);
            // A body that is not JSON leaves no code; the status then explains the failure.
            let answer: Option<Value> = response.json().await.ok();
            let code = answer.as_ref().and_then(|answer| answer["code"].as_i64());
            let limited =
                status == StatusCode::TOO_MANY_REQUESTS || code.is_some_and(|code| RATE_LIMIT_CODES.contains(&code));
            if limited && retries < RATE_LIMIT_RETRIES {
                retries += 1;
                tokio::time::sleep(wait).await;
                continue;
            }
            return match (answer, code) {
                (Some(answer), Some(0)) => Ok(answer),
                (answer, code) => {
                    let detail = answer
                        .as_ref()
                        .and_then(|answer| answer["msg"].as_str())
                        .unwrap_or("no message");
                    Err(match code {
                        Some(code) => {
                            CallError::Refused(format!("feishu {path}: HTTP {status}, code {code}: {detail}"))
                        }
                        // A body that is not JSON, or has no code, does not say how the call went.
                        None => CallError::Failed(ImError::Transport(format!(
                            "feishu {path}: HTTP {status}, no code: {detail}"
                        ))),
                    })
                }
            };
        }
    }
}
