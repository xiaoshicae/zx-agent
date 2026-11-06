//! Feishu: messages and card clicks arrive over the long connection that the official SDKs
//! use; posts go through the open API (docs/serve-im-review.md, section 3.5).
//!
//! The long connection has no published specification. This follows the official Go SDK
//! (`larksuite/oapi-sdk-go`, branch `v3_main` at `db9f52d0`, package `ws`), with one change:
//! events are acknowledged on arrival, not after they are handled, because Feishu redelivers
//! what is not acknowledged within three seconds.
//!
//! The open API calls are in `api.rs`; the long connection in `connection.rs`, with its
//! frames in `frame.rs`; what arrives over it, and the cards, in `events.rs`.

mod api;
mod connection;
mod events;
mod frame;

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Method;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use zx_ai::sync::lock;
use zx_im::{
    Backoff, CARD_CAPACITY, CardLabels, Cards, Destination, ErrorReport, ImChannel, ImError, Inbound, MAX_TEXT_CHARS,
    MessageId, MessageRef, Outbound, Pacer, SEEN_CAPACITY, Seen, card_update_refused, pause, split_text,
};

use api::CallError;
use connection::{ClientConfig, Connection, Ended};
use events::{button, card, markdown, resolution, text_content};

/// Where the open API lives.
const DEFAULT_API_BASE: &str = "https://open.feishu.cn";
/// The tenant token endpoint the documentation recommends; the older
/// `/open-apis/auth/v3/tenant_access_token/internal` is marked as a historical version.
const DEFAULT_TOKEN_URL: &str = "https://accounts.feishu.cn/oauth/v3/token";
const PLATFORM: &str = "feishu";
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
    /// Called with each failed attempt to connect, before the channel waits and tries
    /// again, so that an operator can see a platform that cannot be reached.
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
            request_timeout: Duration::from_secs(30),
            post_interval: Duration::from_millis(200),
            labels: CardLabels::default(),
            on_error: None,
        }
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
    cards: Cards,
}

impl FeishuChannel {
    /// A channel for `config`. Fails when the HTTP client cannot be built.
    pub fn new(config: FeishuConfig) -> Result<Self, ImError> {
        let http = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .build()
            .map_err(|error| ImError::Config(format!("cannot build the HTTP client: {error}")))?;
        Ok(Self {
            posts: Pacer::new(config.post_interval),
            cards: Cards::new(CARD_CAPACITY),
            config,
            http,
            token: tokio::sync::Mutex::new(None),
            direct: Mutex::default(),
        })
    }

    fn report(&self, error: &ImError) {
        if let Some(callback) = &self.config.on_error {
            callback(error);
        }
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

    async fn run(&self, inbound: mpsc::Sender<Inbound>, cancel: &CancellationToken) -> Result<(), ImError> {
        let mut config = ClientConfig::default();
        // The documentation says to deduplicate by `message_id` (im.message.receive_v1).
        let mut seen = Seen::new(SEEN_CAPACITY);
        // Used while the server has set no reconnect interval.
        let mut backoff = Backoff::default();
        loop {
            if cancel.is_cancelled() {
                return Ok(());
            }
            let wait = match self.connect(&mut config).await? {
                Connection::Open(link) => {
                    backoff.reset();
                    match self.receive(*link, &mut config, &inbound, &mut seen, cancel).await {
                        Ended::Cancelled => return Ok(()),
                        // Spread reconnections out, as the server asks.
                        Ended::Lost => connection::jitter(config.nonce),
                    }
                }
                Connection::Later(reason) => {
                    self.report(&ImError::Transport(reason));
                    config.reconnect.unwrap_or_else(|| backoff.advance())
                }
            };
            if !pause(wait, cancel).await {
                return Ok(());
            }
        }
    }

    async fn send(&self, to: &Destination, message: &Outbound) -> Result<MessageRef, ImError> {
        match message {
            Outbound::Text(text) => {
                let (first, rest) = split(text);
                let posted = self.post_text(to, &first).await?;
                // The rest go to the same thread, which a first post in the chat itself starts.
                let thread = posted.thread();
                for part in &rest {
                    self.post_text(&thread, part).await?;
                }
                Ok(posted)
            }
            Outbound::ApprovalRequest {
                id,
                tool,
                summary,
                reason,
            } => {
                let labels = &self.config.labels;
                let request = format!(
                    "**{}**: `{tool}`\n```\n{summary}\n```\n{}: {reason}",
                    labels.title, labels.reason
                );
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
                let posted = self.post(to, "interactive", card).await?;
                self.cards.remember(&posted.id, request);
                Ok(posted)
            }
            Outbound::ApprovalResolved { user, result, .. } => {
                let text = resolution(&self.config.labels, user.as_ref(), *result);
                self.post_text(to, &text).await
            }
        }
    }

    async fn update(&self, posted: &MessageRef, message: &Outbound) -> Result<(), ImError> {
        match message {
            Outbound::Text(text) => {
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
            Outbound::ApprovalRequest { .. } => Err(card_update_refused()),
            Outbound::ApprovalResolved { user, result, .. } => {
                let mut elements = Vec::new();
                if let Some(request) = self.cards.forget(&posted.id) {
                    elements.push(markdown(&request));
                }
                elements.push(markdown(&resolution(&self.config.labels, user.as_ref(), *result)));
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
