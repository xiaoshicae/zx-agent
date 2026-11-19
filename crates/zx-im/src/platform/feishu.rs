//! Feishu: messages and card clicks arrive over the long connection that the official SDKs
//! use; posts go through the open API.
//!
//! The long connection has no published specification. This follows the official Go SDK
//! (`larksuite/oapi-sdk-go`, branch `v3_main` at `db9f52d0`, package `ws`), with one change:
//! events are acknowledged once their messages are kept, not after they are handled, because
//! Feishu redelivers what is not acknowledged within three seconds.
//!
//! The open API calls are in `api.rs`; the long connection in `connection.rs`, with its
//! frames in `frame.rs`; what arrives over it, and the cards, in `events.rs`.

mod api;
mod connection;
mod events;
mod frame;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Method;
use serde::Deserialize;
use serde_json::json;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use zx_util::sync::lock;

use crate::{
    ApprovalCard, CardLabels, Destination, ErrorReport, ImChannel, ImError, Inlet, MessageId, MessageRef, Post,
    Replacement,
};

use super::{
    Backoff, BuiltChannel, DEFAULT_REQUEST_TIMEOUT, MAX_TEXT_CHARS, Pacer, PlatformFactory, PlatformInput, http_client,
    pause, report, secret, split_text,
};
use api::CallError;
use connection::{ClientConfig, Connection, Ended, PREFIX};
use events::{button, card, markdown, resolution, text_content, without_fences, without_mentions};

/// Where the open API lives.
const DEFAULT_API_BASE: &str = "https://open.feishu.cn";
/// The tenant token endpoint the documentation recommends; the older
/// `/open-apis/auth/v3/tenant_access_token/internal` is marked as a historical version.
const DEFAULT_TOKEN_URL: &str = "https://accounts.feishu.cn/oauth/v3/token";
pub(super) const PLATFORM: &str = "feishu";
/// The variables that hold the credentials, unless the `[feishu]` section of im.toml names
/// others.
const DEFAULT_APP_ID_ENV: &str = "FEISHU_APP_ID";
const DEFAULT_APP_SECRET_ENV: &str = "FEISHU_APP_SECRET";
/// Longest interval accepted from the server (pings, reconnection, a frequency limit's
/// wait), so that a bad value cannot stop pings or stall posts.
const MAX_INTERVAL: Duration = Duration::from_secs(3600);

/// Settings of a [`FeishuChannel`]. The secret is sent only to the token and endpoint URLs.
pub struct FeishuConfig {
    pub app_id: String,
    pub app_secret: String,
    /// Open API base URL: `https://open.feishu.cn` unless a proxy or a test server is used.
    pub api_base: String,
    /// Tenant token endpoint: `https://accounts.feishu.cn/oauth/v3/token` by default, the one
    /// the documentation recommends.
    pub token_url: String,
    /// Limit for each API call and for opening the WebSocket.
    pub request_timeout: Duration,
    /// Least time between two posts to one chat: the bots of a group share five messages
    /// per second.
    pub post_interval: Duration,
    pub labels: CardLabels,
    /// Called with what the channel goes on after, so that an operator can see it: each
    /// failed attempt to connect, before the channel waits and tries again, a connection
    /// that went silent, a frame it cannot decode, and an event that is not JSON.
    pub on_error: Option<ErrorReport>,
}

impl FeishuConfig {
    /// Settings for the app `app_id` with `app_secret`, and the defaults of every other field.
    pub fn new(app_id: String, app_secret: String) -> Self {
        Self {
            app_id,
            app_secret,
            api_base: DEFAULT_API_BASE.into(),
            token_url: DEFAULT_TOKEN_URL.into(),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            post_interval: Duration::from_millis(200),
            labels: CardLabels::default(),
            on_error: None,
        }
    }
}

/// The `[feishu]` section of im.toml: which variables hold the app's credentials, and where
/// its APIs are.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct FeishuSection {
    /// Variable holding the App ID of the self-built app.
    #[serde(default = "default_app_id_env")]
    app_id_env: String,
    /// Variable holding the App Secret.
    #[serde(default = "default_app_secret_env")]
    app_secret_env: String,
    /// Open API base URL, for a proxy or a test server.
    api_base: Option<String>,
    /// Tenant token URL, for a proxy or a test server.
    token_url: Option<String>,
}

fn default_app_id_env() -> String {
    DEFAULT_APP_ID_ENV.into()
}

fn default_app_secret_env() -> String {
    DEFAULT_APP_SECRET_ENV.into()
}

/// Feishu, configured by the `[feishu]` section of im.toml.
pub(super) struct Feishu;

impl PlatformFactory for Feishu {
    /// The variable holding the App Secret is the one commands must not see: the App ID is
    /// not a secret. Fails when a variable does not hold its credential, and when the channel
    /// cannot be built.
    fn channel(&self, input: PlatformInput<'_>) -> anyhow::Result<BuiltChannel> {
        let section: FeishuSection = toml::Value::Table(input.section.clone()).try_into()?;
        let mut config = FeishuConfig::new(
            secret(&section.app_id_env, "the Feishu App ID")?,
            secret(&section.app_secret_env, "the Feishu App Secret")?,
        );
        if let Some(base) = section.api_base {
            config.api_base = base;
        }
        if let Some(url) = section.token_url {
            config.token_url = url;
        }
        config.labels.clone_from(input.labels);
        config.on_error = Some(input.on_error);
        let channel = FeishuChannel::new(config).map_err(anyhow::Error::msg)?;
        Ok(BuiltChannel {
            channel: Arc::new(channel),
            hidden_env: vec![section.app_secret_env],
        })
    }
}

/// Feishu through the long connection. Needs a self-built app with the long connection
/// chosen for events and callbacks, the `im.message.receive_v1` event, the
/// `card.action.trigger` callback, and permission to send messages; for groups, only the
/// permission to read messages that mention the bot.
pub struct FeishuChannel {
    config: FeishuConfig,
    http: reqwest::Client,
    /// The tenant token and when it expires. An async lock, held across the renewal, so that
    /// concurrent calls wait for one renewal instead of each starting their own.
    token: tokio::sync::Mutex<Option<(String, Instant)>>,
    posts: Pacer,
    /// Direct chats seen so far: replies there do not open a topic.
    direct: Mutex<HashSet<String>>,
}

/// What a card shows of the request `card`, open and once closed. What the request says
/// mentions nobody, and its summary stays in the code block.
fn shown(labels: &CardLabels, card: &ApprovalCard) -> String {
    format!(
        "**{}**: `{}`\n```\n{}\n```\n{}: {}",
        labels.title,
        without_mentions(&card.tool),
        without_fences(&without_mentions(&card.summary)),
        labels.reason,
        without_mentions(&card.reason)
    )
}

impl FeishuChannel {
    /// A channel for `config`. Fails when the HTTP client cannot be built.
    pub fn new(config: FeishuConfig) -> Result<Self, ImError> {
        Ok(Self {
            http: http_client(config.request_timeout)?,
            posts: Pacer::new(config.post_interval),
            config,
            token: tokio::sync::Mutex::new(None),
            direct: Mutex::default(),
        })
    }

    /// Post `content` of type `msg_type`: a reply in the thread when `to` names one,
    /// otherwise a new message in the chat.
    async fn post(&self, to: &Destination, msg_type: &str, content: String) -> Result<MessageRef, ImError> {
        self.posts.turn(&to.conversation.0).await;
        let answer = match &to.thread {
            Some(root) => {
                let direct = lock(&self.direct).contains(&to.conversation.0);
                let body = json!({"msg_type": msg_type, "content": content, "reply_in_thread": !direct});
                let path = format!("/open-apis/im/v1/messages/{}/reply", root.0);
                self.api(Method::POST, &path, &body).await?
            }
            None => {
                let body = json!({"receive_id": to.conversation.0, "msg_type": msg_type, "content": content});
                self.api(Method::POST, "/open-apis/im/v1/messages?receive_id_type=chat_id", &body)
                    .await?
            }
        };
        let id = answer["data"]["message_id"]
            .as_str()
            .ok_or_else(|| ImError::Transport("feishu: no message_id in the answer".into()))?;
        Ok(MessageRef {
            at: to.clone(),
            id: MessageId(id.into()),
        })
    }

    async fn post_text(&self, to: &Destination, text: &str) -> Result<MessageRef, ImError> {
        self.post(to, "text", text_content(text)).await
    }
}

#[async_trait]
impl ImChannel for FeishuChannel {
    fn platform(&self) -> &str {
        PLATFORM
    }

    async fn run(&self, inlet: Inlet, cancel: &CancellationToken) -> Result<(), ImError> {
        let mut config = ClientConfig::default();
        // Used while the server has set no reconnect interval.
        let mut backoff = Backoff::default();
        loop {
            let connected = tokio::select! {
                biased;
                () = cancel.cancelled() => return Ok(()),
                connected = self.connect(&mut config) => connected?,
            };
            let wait = match connected {
                Connection::Open(link) => {
                    backoff.reset();
                    match self.receive(*link, &mut config, &inlet, cancel).await {
                        Ended::Cancelled => return Ok(()),
                        // Spread reconnections out, as the server asks.
                        Ended::Lost => connection::jitter(config.nonce),
                        Ended::Silent(silence) => {
                            let silent = format!("{PREFIX}: silent for {} s; opening it again", silence.as_secs());
                            report(self.config.on_error.as_ref(), &ImError::Transport(silent));
                            connection::jitter(config.nonce)
                        }
                    }
                }
                Connection::Later(reason) => {
                    report(self.config.on_error.as_ref(), &ImError::Transport(reason));
                    config.reconnect.unwrap_or_else(|| backoff.advance())
                }
            };
            if !pause(wait, cancel).await {
                return Ok(());
            }
        }
    }

    async fn send(&self, to: &Destination, message: &Post) -> Result<MessageRef, ImError> {
        match message {
            Post::Text(text) => {
                let (first, rest) = split(text);
                let posted = self.post_text(to, &first).await?;
                // The rest go to the same thread, which a first post in the chat itself starts.
                let thread = posted.thread();
                for part in &rest {
                    self.post_text(&thread, part).await?;
                }
                Ok(posted)
            }
            Post::ApprovalRequest { id, card: asked } => {
                let labels = &self.config.labels;
                let request = shown(labels, asked);
                // The click carries everything needed to find the conversation.
                let value = |choice: &str| {
                    json!({
                        "zx": choice,
                        "approval": id.0,
                        "chat": to.conversation.0,
                        "thread": to.thread.as_ref().map(|thread| &thread.0),
                    })
                };
                let card = card(&[
                    markdown(&request),
                    button(&labels.allow_once, "primary", &value("allow_once")),
                    button(&labels.deny, "danger", &value("deny")),
                ]);
                self.post(to, "interactive", card).await
            }
        }
    }

    async fn update(&self, posted: &MessageRef, replacement: &Replacement) -> Result<(), ImError> {
        match replacement {
            Replacement::Text(text) => {
                let (first, rest) = split(text);
                let thread = posted.thread();
                let path = format!("/open-apis/im/v1/messages/{}", posted.id.0);
                let body = json!({"msg_type": "text", "content": text_content(&first)});
                match self.api(Method::PUT, &path, &body).await {
                    Ok(_) => {}
                    // Feishu refused the edit (a message allows 20 edits, within a time the
                    // tenant's admin sets): post the text instead of losing it.
                    Err(CallError::Api { code: Some(_), .. }) => {
                        self.post_text(&thread, &first).await?;
                    }
                    Err(error) => return Err(error.into()),
                }
                for part in &rest {
                    self.post_text(&thread, part).await?;
                }
                Ok(())
            }
            Replacement::ApprovalResolved {
                card: asked,
                user,
                result,
                ..
            } => {
                let labels = &self.config.labels;
                let elements = vec![
                    markdown(&shown(labels, asked)),
                    markdown(&resolution(labels, user.as_ref(), *result)),
                ];
                let path = format!("/open-apis/im/v1/messages/{}", posted.id.0);
                self.api(Method::PATCH, &path, &json!({"content": card(&elements)}))
                    .await
                    .map(drop)
                    .map_err(ImError::from)
            }
        }
    }
}

/// Text in parts of at most one message: the first part and the rest.
fn split(text: &str) -> (String, Vec<String>) {
    split_text(text, MAX_TEXT_CHARS, String::push)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_feishu_section_defaults_its_variables() {
        let section: FeishuSection = toml::from_str("").unwrap();
        assert_eq!(
            (section.app_id_env.as_str(), section.app_secret_env.as_str()),
            (DEFAULT_APP_ID_ENV, DEFAULT_APP_SECRET_ENV)
        );
    }
}
