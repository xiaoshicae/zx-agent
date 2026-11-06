//! The open API: the tenant token, calls with frequency limits waited out, and why a call
//! did not succeed.

use std::time::Duration;

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tokio::time::Instant;
use zx_im::{ImError, RATE_LIMIT_RETRIES};

use super::{FeishuChannel, MAX_INTERVAL};

/// A token is renewed when less than this remains; the Go SDK renews three minutes early.
const TOKEN_MARGIN: Duration = Duration::from_secs(5 * 60);
/// Longest token lifetime accepted; tenant tokens last at most two hours.
const MAX_TOKEN_LIFETIME: Duration = Duration::from_secs(24 * 3600);
/// Codes that mean a frequency limit: 99991400 from the gateway (frequency-control guide)
/// and 230020 from the message APIs.
const RATE_LIMIT_CODES: [i64; 2] = [99_991_400, 230_020];
/// Header with the seconds to wait after a frequency limit.
const RATE_LIMIT_RESET: &str = "x-ogw-ratelimit-reset";

/// Why an API call did not succeed.
pub(crate) enum CallError {
    /// No token: the secret was refused, or the token endpoint failed.
    Token(ImError),
    /// The call failed; `code` is what Feishu answered, `None` when no answer came.
    Api { code: Option<i64>, message: String },
}

impl From<CallError> for ImError {
    fn from(error: CallError) -> Self {
        match error {
            CallError::Token(error) => error,
            CallError::Api { message, .. } => ImError::Transport(message),
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

    /// Call an open API method. A frequency limit is waited out and retried.
    pub(crate) async fn api(&self, method: Method, path: &str, body: &Value) -> Result<Value, CallError> {
        let url = format!("{}{path}", self.config.api_base.trim_end_matches('/'));
        let mut retries = 0;
        loop {
            let token = self.token().await.map_err(CallError::Token)?;
            let response = self
                .http
                .request(method.clone(), &url)
                .bearer_auth(&token)
                .json(body)
                .send()
                .await
                .map_err(|error| CallError::Api {
                    code: None,
                    message: format!("feishu {path}: {error}"),
                })?;
            let status = response.status();
            let wait = rate_limit_wait(&response);
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
                        .unwrap_or("no message")
                        .to_string();
                    Err(CallError::Api {
                        code,
                        message: format!("feishu {path}: HTTP {status}, code {code:?}: {detail}"),
                    })
                }
            };
        }
    }
}

/// The wait a frequency limit asks for; one second when the header is missing or
/// unreadable, at most [`MAX_INTERVAL`].
fn rate_limit_wait(response: &reqwest::Response) -> Duration {
    let secs = response
        .headers()
        .get(RATE_LIMIT_RESET)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(1);
    Duration::from_secs(secs).min(MAX_INTERVAL)
}
