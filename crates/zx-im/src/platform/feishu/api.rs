//! The open API: the tenant token, calls with frequency limits waited out, and why a call
//! did not succeed.

use std::time::Duration;

use reqwest::header::CONTENT_TYPE;
use reqwest::{Method, RequestBuilder, StatusCode, Url};
use serde_json::{Value, json};
use tokio::time::Instant;
use zx_util::random::random_u64;

use super::FeishuChannel;
use crate::platform::{RATE_LIMIT_RETRIES, RateLimitWaits, asked_wait, body_within, content_type_is};
use crate::{Attachment, ImError};

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
            .post(self.token_url.clone())
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
        let url = self.api_base.at(path);
        let token = self.token().await?;
        let response = self
            .http
            .get(url)
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
        let url = self.url(path, args);
        self.answer(path, |token| {
            let request = self.http.request(method.clone(), url.clone()).bearer_auth(token);
            match body {
                Some(body) => request.json(body),
                None => request,
            }
        })
        .await
    }

    /// Upload `file` for a file message (`im/v1/files`, needs `im:resource` or
    /// `im:resource:upload`): the key that names it there. Uploaded as a `stream`, the type
    /// for any file.
    pub(super) async fn upload(&self, file: &Attachment) -> Result<String, ImError> {
        const PATH: &str = "/open-apis/im/v1/files";
        let url = self.url(PATH, &[]);
        let fields = [("file_type", "stream"), ("file_name", file.name.as_str())];
        let form = Multipart::new(&fields, &file.name, &file.bytes);
        let answer = self
            .answer(PATH, |token| {
                self.http
                    .post(url.clone())
                    .bearer_auth(token)
                    .header(CONTENT_TYPE, &form.content_type)
                    .body(form.body.clone())
            })
            .await?;
        answer["data"]["file_key"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| ImError::Transport(format!("feishu {PATH}: no file_key in the answer")))
    }

    /// The address of the open API `path`, with `args` in its query.
    fn url(&self, path: &str, args: &[(&str, &str)]) -> Url {
        let mut url = self.api_base.at(path);
        if !args.is_empty() {
            url.query_pairs_mut().extend_pairs(args);
        }
        url
    }

    /// The answer to the request that `request` makes with the tenant token to `path`. A
    /// frequency limit is waited out and the request made again, within `RateLimitWaits`; a
    /// code other than 0 is a refusal.
    async fn answer(&self, path: &str, request: impl Fn(&str) -> RequestBuilder) -> Result<Value, CallError> {
        let mut waits = RateLimitWaits::new(RATE_LIMIT_RETRIES);
        loop {
            let token = self.token().await.map_err(CallError::Failed)?;
            let response = request(&token)
                .send()
                .await
                .map_err(|error| CallError::Failed(ImError::Transport(format!("feishu {path}: {error}"))))?;
            let status = response.status();
            let asked = asked_wait(response.headers(), RATE_LIMIT_RESET);
            // A body that is not JSON leaves no code; the status then explains the failure.
            let answer: Option<Value> = response.json().await.ok();
            let code = answer.as_ref().and_then(|answer| answer["code"].as_i64());
            let limited =
                status == StatusCode::TOO_MANY_REQUESTS || code.is_some_and(|code| RATE_LIMIT_CODES.contains(&code));
            if limited {
                let what = format!("feishu {path}");
                let wait = waits
                    .next(asked, &what, self.config.on_error.as_ref())
                    .map_err(|why| CallError::Failed(ImError::Transport(format!("{what}: {why}"))))?;
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

/// A `multipart/form-data` body (RFC 7578): text fields, then one file, under `file`.
struct Multipart {
    /// The body's type, with the boundary between its parts.
    content_type: String,
    body: Vec<u8>,
}

impl Multipart {
    /// The text `fields`, then the file `name` with its `bytes`.
    fn new(fields: &[(&str, &str)], name: &str, bytes: &[u8]) -> Self {
        // Random, so that no part holds it: "the boundary delimiter MUST NOT appear inside
        // any of the encapsulated parts" (RFC 2046, 5.1.1).
        let boundary = format!("zx-im-{:016x}{:016x}", random_u64(), random_u64());
        let mut body = Vec::with_capacity(bytes.len() + 512);
        for (field, value) in fields {
            let part = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"\r\n\r\n{value}\r\n");
            body.extend_from_slice(part.as_bytes());
        }
        let head = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{}\"\r\nContent-Type: application/octet-stream\r\n\r\n",
            quoted(name)
        );
        body.extend_from_slice(head.as_bytes());
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        Self {
            content_type: format!("multipart/form-data; boundary={boundary}"),
            body,
        }
    }
}

/// `name` as the value of a quoted parameter: the quote, the backslash and line breaks, which
/// would end it or the header, become `_`.
fn quoted(name: &str) -> String {
    name.chars()
        .map(|c| if matches!(c, '"' | '\\' | '\r' | '\n') { '_' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_multipart_body_holds_the_fields_then_the_file_between_boundaries() {
        let form = Multipart::new(
            &[("file_type", "stream"), ("file_name", "报告.md")],
            "报告.md",
            b"# done",
        );
        let boundary = form
            .content_type
            .strip_prefix("multipart/form-data; boundary=")
            .unwrap();
        let body = String::from_utf8(form.body).unwrap();
        let expected = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file_type\"\r\n\r\nstream\r\n\
             --{boundary}\r\nContent-Disposition: form-data; name=\"file_name\"\r\n\r\n报告.md\r\n\
             --{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"报告.md\"\r\n\
             Content-Type: application/octet-stream\r\n\r\n# done\r\n--{boundary}--\r\n"
        );
        assert_eq!(body, expected);
        // Each body has a boundary of its own.
        let other = Multipart::new(&[], "a", b"");
        assert_ne!(other.content_type, form.content_type);
    }

    #[test]
    fn a_file_name_cannot_end_its_quoted_parameter() {
        assert_eq!(quoted("a\"b\\c\r\nd.txt"), "a_b_c__d.txt");
    }
}
