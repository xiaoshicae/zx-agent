//! The Web API: calls with a token, rate limits waited out, and Slack's refusals told apart
//! from calls that got no answer.

use reqwest::header::RETRY_AFTER;
use reqwest::{StatusCode, Url};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::{HISTORY_PAGE_MESSAGES, SETUP_ERRORS, SlackChannel};
use crate::platform::{RATE_LIMIT_RETRIES, rate_limit_wait};
use crate::{Destination, ImError, MessageId, MessageRef, ThreadId};

impl SlackChannel {
    /// Call a Web API method with `token`, its arguments in the query (`args`) or in a JSON
    /// `body`. A rate limit is waited out and retried. As an [`ImError`], a refusal that
    /// retrying cannot fix is a configuration error.
    pub(super) async fn call(
        &self,
        method: &str,
        token: &str,
        args: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<Value, CallError> {
        self.call_retrying(method, token, args, body, RATE_LIMIT_RETRIES).await
    }

    /// [`Self::call`], waiting out a rate limit at most `rate_limit_retries` times: after
    /// those, or with none, the limit is the call's error.
    async fn call_retrying(
        &self,
        method: &str,
        token: &str,
        args: &[(&str, &str)],
        body: Option<&Value>,
        rate_limit_retries: u32,
    ) -> Result<Value, CallError> {
        let base = self.config.api_base.trim_end_matches('/');
        let mut url = Url::parse(&format!("{base}/{method}")).map_err(|error| {
            CallError::Failed(ImError::Config(format!(
                "slack {method}: invalid API base {base}: {error}"
            )))
        })?;
        if !args.is_empty() {
            url.query_pairs_mut().extend_pairs(args);
        }
        let mut retries = 0;
        loop {
            let request = self.http.post(url.clone()).bearer_auth(token);
            let request = match body {
                Some(body) => request.json(body),
                None => request,
            };
            let response = request
                .send()
                .await
                .map_err(|error| CallError::Failed(ImError::Transport(format!("slack {method}: {error}"))))?;
            let status = response.status();
            if status == StatusCode::TOO_MANY_REQUESTS && retries < rate_limit_retries {
                retries += 1;
                let wait = rate_limit_wait(response.headers(), RETRY_AFTER);
                tokio::time::sleep(wait).await;
                continue;
            }
            let answer: Value = response.json().await.map_err(|error| {
                CallError::Failed(ImError::Transport(format!("slack {method}: HTTP {status}: {error}")))
            })?;
            if answer["ok"] == Value::Bool(true) {
                return Ok(answer);
            }
            return Err(CallError::Refused {
                method: method.to_string(),
                code: answer["error"].as_str().unwrap_or("no error code").to_string(),
            });
        }
    }

    /// A Web API call without a body, given up when `cancel` is triggered: `None`.
    pub(super) async fn call_unless_cancelled(
        &self,
        method: &str,
        token: &str,
        cancel: &CancellationToken,
    ) -> Option<Result<Value, ImError>> {
        tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            answer = self.call(method, token, &[], None) => Some(answer.map_err(ImError::from)),
        }
    }

    /// One page of the thread `thread` of `at`'s chat: the one `cursor` names, or the first
    /// when it is empty. A rate limit is waited out at most `rate_limit_retries` times.
    pub(super) async fn replies_page(
        &self,
        at: &Destination,
        thread: &ThreadId,
        cursor: &str,
        rate_limit_retries: u32,
    ) -> Result<Value, ImError> {
        let page_size = HISTORY_PAGE_MESSAGES.to_string();
        let mut args = vec![
            ("channel", at.conversation.0.as_str()),
            ("ts", thread.0.as_str()),
            ("limit", page_size.as_str()),
        ];
        if !cursor.is_empty() {
            args.push(("cursor", cursor));
        }
        let token = &self.config.bot_token;
        self.call_retrying("conversations.replies", token, &args, None, rate_limit_retries)
            .await
            .map_err(ImError::from)
    }

    pub(super) async fn post(
        &self,
        to: &Destination,
        text: &str,
        blocks: Option<Value>,
    ) -> Result<MessageRef, ImError> {
        self.posts.turn(&to.conversation.0).await;
        let mut body = Map::new();
        body.insert("channel".into(), json!(to.conversation.0));
        body.insert("text".into(), json!(text));
        if let Some(thread) = &to.thread {
            body.insert("thread_ts".into(), json!(thread.0));
        }
        if let Some(blocks) = blocks {
            body.insert("blocks".into(), blocks);
        }
        let answer = self
            .call(
                "chat.postMessage",
                &self.config.bot_token,
                &[],
                Some(&Value::Object(body)),
            )
            .await?;
        let ts = answer["ts"]
            .as_str()
            .ok_or_else(|| ImError::Transport("slack chat.postMessage: no ts in the answer".into()))?;
        Ok(MessageRef {
            at: to.clone(),
            id: MessageId(ts.into()),
        })
    }

    /// Replace a posted message. Without `blocks`, Slack removes the message's blocks and
    /// shows `text` (chat.update: "If the `text` argument is provided and `blocks` are not
    /// provided, the `blocks` will be removed").
    pub(super) async fn edit(&self, posted: &MessageRef, text: &str, blocks: Option<Value>) -> Result<(), CallError> {
        let mut body = Map::new();
        body.insert("channel".into(), json!(posted.at.conversation.0));
        body.insert("ts".into(), json!(posted.id.0));
        body.insert("text".into(), json!(text));
        if let Some(blocks) = blocks {
            body.insert("blocks".into(), blocks);
        }
        let body = Value::Object(body);
        self.call("chat.update", &self.config.bot_token, &[], Some(&body))
            .await
            .map(drop)
    }
}

/// Why a Web API call failed.
pub(super) enum CallError {
    /// Slack answered with `ok: false`: it refused the call, for the reason `code` names.
    Refused { method: String, code: String },
    /// No answer came that says how the call went.
    Failed(ImError),
}

impl From<CallError> for ImError {
    /// A refusal that retrying cannot fix is a configuration error.
    fn from(error: CallError) -> Self {
        match error {
            CallError::Refused { method, code } if SETUP_ERRORS.contains(&code.as_str()) => {
                ImError::Config(format!("slack {method}: {code}"))
            }
            CallError::Refused { method, code } => ImError::Transport(format!("slack {method}: {code}")),
            CallError::Failed(error) => error,
        }
    }
}
