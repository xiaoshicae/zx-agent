//! Feishu: messages and card clicks arrive over the long connection that the official SDKs
//! use; posts go through the open API (docs/serve-im-review.md, section 3.5).
//!
//! The long connection has no published specification. This follows the official Go SDK
//! (`larksuite/oapi-sdk-go`, branch `v3_main` at `db9f52d0`, package `ws`), with one change:
//! events are acknowledged on arrival, not after they are handled, because Feishu redelivers
//! what is not acknowledged within three seconds.

mod frame;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_util::sync::CancellationToken;

use crate::agent::ApprovalResult;
use crate::sync::lock;

use super::{
    ApprovalId, CardLabels, Choice, ConversationId, Destination, ImChannel, ImError, Inbound, MessageId, MessageRef,
    Outbound, Seen, ThreadId, UserId,
};
use frame::Frame;

/// Where the open API lives.
pub const DEFAULT_API_BASE: &str = "https://open.feishu.cn";
/// The tenant token endpoint the documentation recommends; the older
/// `/open-apis/auth/v3/tenant_access_token/internal` is marked as a historical version.
pub const DEFAULT_TOKEN_URL: &str = "https://accounts.feishu.cn/oauth/v3/token";
const PLATFORM: &str = "feishu";
/// Where the long connection's address is requested (Go SDK `ws/const.go`).
const ENDPOINT_PATH: &str = "/callback/ws/endpoint";
/// Frame methods (Go SDK `ws/const.go`): control frames carry pings and pongs.
const CONTROL: i32 = 0;
const DATA: i32 = 1;
/// Endpoint codes the Go SDK retries: 1 (system busy) and 1000040343. Any other non-zero
/// code ends the client (`ws/client_transport.go`).
const RETRY_CODES: [i64; 2] = [1, 1000040343];
/// Handshake refusal for too many connections, with status 514 (`ws/const.go`).
const EXCEED_CONN_LIMIT: &str = "1000040350";
/// Ping interval until the server sends its own (`ws/client.go`).
const DEFAULT_PING_INTERVAL: Duration = Duration::from_secs(120);
/// Longest interval accepted from the server, so that a bad value cannot stop pings.
const MAX_INTERVAL: Duration = Duration::from_secs(3600);
/// Silence that counts as a dead connection: twice the ping interval plus this
/// (`ws/client_session.go`).
const DEADLINE_MARGIN: Duration = Duration::from_secs(5);
/// Waits after failed connection attempts while the server has set no reconnect interval.
const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(60);
/// Fragments of one message wait this long for the rest; the Go SDK keeps them for five
/// seconds after each fragment (`ws/client_message.go`).
const FRAGMENT_TTL: Duration = Duration::from_secs(5);
/// Most messages with fragments waiting at once, and most fragments of one message.
const MAX_PARTIAL: usize = 64;
const MAX_FRAGMENTS: usize = 64;
/// Messages remembered to drop redeliveries; the documentation says to deduplicate by
/// `message_id` (im.message.receive_v1).
const SEEN_CAPACITY: usize = 1024;
/// A token is renewed when less than this remains; the Go SDK renews three minutes early.
const TOKEN_MARGIN: Duration = Duration::from_secs(5 * 60);
/// Longest token lifetime accepted; tenant tokens last at most two hours.
const MAX_TOKEN_LIFETIME: Duration = Duration::from_secs(24 * 3600);
/// A rate-limited call is retried this many times after waiting as told.
const MAX_RATE_LIMIT_RETRIES: u32 = 3;
/// Codes that mean a frequency limit: 99991400 from the gateway (frequency-control guide)
/// and 230020 from the message APIs.
const RATE_LIMIT_CODES: [i64; 2] = [99991400, 230020];
/// Header with the seconds to wait after a frequency limit.
const RATE_LIMIT_RESET: &str = "x-ogw-ratelimit-reset";
/// Characters per posted message; longer text is split. Requests may reach 150 KB, but
/// long messages read badly; the Slack channel uses the same length.
const MAX_TEXT_CHARS: usize = 4000;
/// Cards remembered so that closing one keeps the request it showed.
const CARD_CAPACITY: usize = 256;
/// What the acknowledgement of an event carries: the Go SDK's answer for a handled event.
const ACK_PAYLOAD: &[u8] = br#"{"code":200,"headers":null,"data":null}"#;

/// Settings of a [`FeishuChannel`]. The secret is sent only to the token and endpoint URLs.
pub struct FeishuConfig {
    pub app_id: String,
    pub app_secret: String,
    /// Open API base URL, [`DEFAULT_API_BASE`] unless a proxy or a test server is used.
    pub api_base: String,
    /// Tenant token endpoint, [`DEFAULT_TOKEN_URL`] by default.
    pub token_url: String,
    /// Limit for each API call and for opening the WebSocket.
    pub request_timeout: Duration,
    /// Least time between two posts to one chat: the bots of a group share five messages
    /// per second.
    pub post_interval: Duration,
    pub labels: CardLabels,
}

impl FeishuConfig {
    pub fn new(app_id: String, app_secret: String) -> Self {
        Self {
            app_id,
            app_secret,
            api_base: DEFAULT_API_BASE.into(),
            token_url: DEFAULT_TOKEN_URL.into(),
            request_timeout: Duration::from_secs(30),
            post_interval: Duration::from_millis(200),
            labels: CardLabels::default(),
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
    /// When each chat may be posted to next; sends reserve their turn under the lock.
    next_post: Mutex<HashMap<String, Instant>>,
    /// Direct chats seen so far: replies there do not open a topic.
    direct: Mutex<HashSet<String>>,
    /// The request text of open cards, so that closing a card keeps it.
    cards: Mutex<VecDeque<(MessageId, String)>>,
}

/// Why an API call did not succeed.
enum CallError {
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

/// Settings the server sends with the address and in pongs.
#[derive(Clone, Copy)]
struct ClientConfig {
    ping: Duration,
    /// Wait between reconnection attempts; until the server sets it, a backoff is used.
    reconnect: Option<Duration>,
    /// Random delay before the first reconnection, at most this long.
    nonce: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            ping: DEFAULT_PING_INTERVAL,
            reconnect: None,
            nonce: Duration::ZERO,
        }
    }
}

impl ClientConfig {
    /// Apply what `value` sets. A missing or zero interval keeps the current one: the Go
    /// SDK would set it to zero.
    fn update(&mut self, value: &Value) {
        let seconds = |key: &str| {
            value[key]
                .as_u64()
                .filter(|secs| *secs > 0)
                .map(|secs| Duration::from_secs(secs).min(MAX_INTERVAL))
        };
        if let Some(ping) = seconds("PingInterval") {
            self.ping = ping;
        }
        if let Some(reconnect) = seconds("ReconnectInterval") {
            self.reconnect = Some(reconnect);
        }
        if let Some(nonce) = value["ReconnectNonce"].as_u64() {
            self.nonce = Duration::from_secs(nonce).min(MAX_INTERVAL);
        }
    }

    fn deadline(&self) -> Instant {
        Instant::now() + self.ping * 2 + DEADLINE_MARGIN
    }
}

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

enum Ended {
    Cancelled,
    /// The connection dropped or went silent.
    Lost,
}

impl FeishuChannel {
    pub fn new(config: FeishuConfig) -> Result<Self, ImError> {
        let http = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .build()
            .map_err(|error| ImError::Config(format!("cannot build the HTTP client: {error}")))?;
        Ok(Self {
            config,
            http,
            token: tokio::sync::Mutex::new(None),
            next_post: Mutex::default(),
            direct: Mutex::default(),
            cards: Mutex::default(),
        })
    }

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
    async fn api(&self, method: Method, path: &str, body: &Value) -> Result<Value, CallError> {
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
            if limited && retries < MAX_RATE_LIMIT_RETRIES {
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

    /// Wait for this chat's next posting turn.
    async fn turn(&self, conversation: &str) {
        let at = {
            let mut next = lock(&self.next_post);
            let now = Instant::now();
            let at = next.get(conversation).copied().filter(|at| *at > now).unwrap_or(now);
            next.insert(conversation.to_string(), at + self.config.post_interval);
            at
        };
        tokio::time::sleep_until(at).await;
    }

    /// Post `content` of type `msg_type`: a reply in the thread when `to` names one,
    /// otherwise a new message in the chat.
    async fn post(&self, to: &Destination, msg_type: &str, content: String) -> Result<MessageRef, ImError> {
        self.turn(&to.conversation.0).await;
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

    fn remember_card(&self, card: &MessageId, request: String) {
        let mut cards = lock(&self.cards);
        cards.push_back((card.clone(), request));
        if cards.len() > CARD_CAPACITY {
            cards.pop_front();
        }
    }

    fn forget_card(&self, card: &MessageId) -> Option<String> {
        let mut cards = lock(&self.cards);
        let index = cards.iter().position(|(id, _)| id == card)?;
        cards.remove(index).map(|(_, request)| request)
    }

    /// Ask for the long connection's address and open it. `Ok(None)` is a failure worth
    /// retrying; an error is one that retrying cannot fix.
    async fn connect(&self, config: &mut ClientConfig) -> Result<Option<(Socket, i32)>, ImError> {
        let url = format!("{}{ENDPOINT_PATH}", self.config.api_base.trim_end_matches('/'));
        let body = json!({
            "AppID": self.config.app_id,
            "AppSecret": self.config.app_secret,
            "ClientAssertion": "",
        });
        let response = match self.http.post(&url).header("locale", "zh").json(&body).send().await {
            Ok(response) if response.status() == StatusCode::OK => response,
            Ok(_) | Err(_) => return Ok(None),
        };
        let Ok(answer) = response.json::<Value>().await else {
            return Ok(None);
        };
        match answer["code"].as_i64() {
            Some(0) => {}
            Some(code) if RETRY_CODES.contains(&code) => return Ok(None),
            code => {
                let detail = answer["msg"].as_str().unwrap_or("no message");
                return Err(ImError::Config(format!(
                    "feishu long connection: code {code:?}: {detail}"
                )));
            }
        }
        // The address carries a ticket: it is used, never shown.
        let Some(address) = answer["data"]["URL"].as_str() else {
            return Ok(None);
        };
        config.update(&answer["data"]["ClientConfig"]);
        let service = service_id(address);
        match tokio::time::timeout(self.config.request_timeout, tokio_tungstenite::connect_async(address)).await {
            Ok(Ok((socket, _))) => Ok(Some((socket, service))),
            Ok(Err(tungstenite::Error::Http(response))) => {
                let status = response.status();
                let auth = response
                    .headers()
                    .get("Handshake-Autherrcode")
                    .and_then(|value| value.to_str().ok());
                if status == StatusCode::FORBIDDEN || (status.as_u16() == 514 && auth == Some(EXCEED_CONN_LIMIT)) {
                    Err(ImError::Config(format!(
                        "feishu long connection refused: HTTP {status}, auth code {}",
                        auth.unwrap_or("none")
                    )))
                } else {
                    Ok(None)
                }
            }
            Ok(Err(_)) | Err(_) => Ok(None),
        }
    }

    /// Receive over one connection until it ends.
    async fn receive(
        &self,
        mut socket: Socket,
        service: i32,
        config: &mut ClientConfig,
        inbound: &mpsc::Sender<Inbound>,
        seen: &mut Seen<String>,
        cancel: &CancellationToken,
    ) -> Ended {
        let mut fragments = Fragments::default();
        // The first ping goes out at once (`ws/client_session.go`).
        let mut last_ping = Instant::now();
        let mut next_ping = last_ping;
        let mut deadline = config.deadline();
        loop {
            let message = tokio::select! {
                () = cancel.cancelled() => {
                    // The server is stopping; a failed close changes nothing.
                    socket.close(None).await.ok();
                    return Ended::Cancelled;
                }
                () = tokio::time::sleep_until(next_ping) => {
                    let ping = Frame {
                        service,
                        method: CONTROL,
                        headers: vec![("type".into(), "ping".into())],
                        ..Frame::default()
                    };
                    if socket.send(Message::binary(ping.encode())).await.is_err() {
                        return Ended::Lost;
                    }
                    last_ping = Instant::now();
                    next_ping = last_ping + config.ping;
                    continue;
                }
                () = tokio::time::sleep_until(deadline) => return Ended::Lost,
                message = socket.next() => message,
            };
            let bytes = match message {
                Some(Ok(Message::Binary(bytes))) => bytes,
                Some(Ok(Message::Close(_)) | Err(_)) | None => return Ended::Lost,
                // The Go SDK reads only binary messages; anything else still shows life.
                Some(Ok(Message::Text(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {
                    deadline = config.deadline();
                    continue;
                }
            };
            deadline = config.deadline();
            let Ok(frame) = Frame::decode(&bytes) else {
                continue;
            };
            match (frame.method, frame.header("type")) {
                (CONTROL, Some("pong")) if !frame.payload.is_empty() => {
                    if let Ok(value) = serde_json::from_slice::<Value>(&frame.payload) {
                        config.update(&value);
                        // A new interval counts from the last ping, not after the old one.
                        next_ping = last_ping + config.ping;
                    }
                }
                (DATA, Some("event")) => {
                    let Some(payload) = fragments.add(&frame, Instant::now()) else {
                        continue;
                    };
                    // Acknowledge first: Feishu redelivers what is not acknowledged in time.
                    if socket
                        .send(Message::binary(acknowledge(&frame).encode()))
                        .await
                        .is_err()
                    {
                        return Ended::Lost;
                    }
                    for event in self.events(&payload, seen) {
                        if inbound.send(event).await.is_err() {
                            // The server no longer reads: it is stopping.
                            return Ended::Cancelled;
                        }
                    }
                }
                // Pings from the server, empty pongs, and `card` frames, which the Go SDK
                // drops: card clicks arrive as events.
                _ => {}
            }
        }
    }

    /// The messages and clicks in one event.
    fn events(&self, payload: &[u8], seen: &mut Seen<String>) -> Vec<Inbound> {
        let Ok(value) = serde_json::from_slice::<Value>(payload) else {
            return Vec::new();
        };
        match value["header"]["event_type"].as_str() {
            Some("im.message.receive_v1") => match message(&value["event"]) {
                Some(addressed) if seen.insert(&addressed.id) => {
                    if addressed.direct
                        && let Inbound::Message { at, .. } = &addressed.inbound
                    {
                        lock(&self.direct).insert(at.conversation.0.clone());
                    }
                    vec![addressed.inbound]
                }
                _ => Vec::new(),
            },
            Some("card.action.trigger") => decision(&value["event"]).into_iter().collect(),
            _ => Vec::new(),
        }
    }
}

#[async_trait]
impl ImChannel for FeishuChannel {
    fn platform(&self) -> &str {
        PLATFORM
    }

    async fn run(&self, inbound: mpsc::Sender<Inbound>, cancel: &CancellationToken) -> Result<(), ImError> {
        let mut config = ClientConfig::default();
        let mut seen = Seen::new(SEEN_CAPACITY);
        let mut backoff = RECONNECT_MIN;
        loop {
            if cancel.is_cancelled() {
                return Ok(());
            }
            let wait = match self.connect(&mut config).await? {
                Some((socket, service)) => {
                    backoff = RECONNECT_MIN;
                    match self
                        .receive(socket, service, &mut config, &inbound, &mut seen, cancel)
                        .await
                    {
                        Ended::Cancelled => return Ok(()),
                        // Spread reconnections out, as the server asks.
                        Ended::Lost => jitter(config.nonce),
                    }
                }
                None => {
                    let wait = config.reconnect.unwrap_or(backoff);
                    backoff = (backoff * 2).min(RECONNECT_MAX);
                    wait
                }
            };
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                () = tokio::time::sleep(wait) => {}
            }
        }
    }

    async fn send(&self, to: &Destination, message: &Outbound) -> Result<MessageRef, ImError> {
        match message {
            Outbound::Text(text) => {
                let mut parts = split(text).into_iter();
                let first = parts.next().unwrap_or_default();
                let posted = self.post_text(to, &first).await?;
                // The rest go to the same thread, which a first post in the chat itself starts.
                let rest_to = Destination {
                    conversation: to.conversation.clone(),
                    thread: Some(to.thread.clone().unwrap_or_else(|| ThreadId(posted.id.0.clone()))),
                };
                for part in parts {
                    self.post_text(&rest_to, &part).await?;
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
                    "**{}**：`{tool}`\n```\n{summary}\n```\n{}：{reason}",
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
                let card = card(vec![
                    markdown(&request),
                    button(&labels.allow_once, "primary", value("allow_once")),
                    button(&labels.deny, "danger", value("deny")),
                ]);
                let posted = self.post(to, "interactive", card).await?;
                self.remember_card(&posted.id, request);
                Ok(posted)
            }
            Outbound::ApprovalResolved { .. } => self.post_text(to, &resolution(&self.config.labels, message)).await,
        }
    }

    async fn update(&self, posted: &MessageRef, message: &Outbound) -> Result<(), ImError> {
        match message {
            Outbound::Text(text) => {
                let mut parts = split(text).into_iter();
                let first = parts.next().unwrap_or_default();
                let thread = Destination {
                    conversation: posted.at.conversation.clone(),
                    thread: Some(
                        posted
                            .at
                            .thread
                            .clone()
                            .unwrap_or_else(|| ThreadId(posted.id.0.clone())),
                    ),
                };
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
                for part in parts {
                    self.post_text(&thread, &part).await?;
                }
                Ok(())
            }
            Outbound::ApprovalRequest { .. } => Err(ImError::Transport(
                "an approval request is posted, not written over a message".into(),
            )),
            Outbound::ApprovalResolved { .. } => {
                let mut elements = Vec::new();
                if let Some(request) = self.forget_card(&posted.id) {
                    elements.push(markdown(&request));
                }
                elements.push(markdown(&resolution(&self.config.labels, message)));
                let path = format!("/open-apis/im/v1/messages/{}", posted.id.0);
                self.api(Method::PATCH, &path, &json!({"content": card(elements)}))
                    .await
                    .map(drop)
                    .map_err(ImError::from)
            }
        }
    }
}

/// A message addressed to the bot, with its ID and whether it came from a direct chat.
struct Addressed {
    id: String,
    inbound: Inbound,
    direct: bool,
}

/// A text message from a person: in a direct chat, or in a group with a bot mentioned.
fn message(event: &Value) -> Option<Addressed> {
    if event["sender"]["sender_type"] != "user" {
        return None;
    }
    let message = &event["message"];
    if message["message_type"] != "text" {
        return None;
    }
    let direct = match message["chat_type"].as_str()? {
        "p2p" => true,
        "group" => false,
        _ => return None,
    };
    let mentions = message["mentions"].as_array().map(Vec::as_slice).unwrap_or_default();
    // With only the permission to read mentions of the bot, every group message delivered
    // mentions it; the check also keeps other group messages out.
    if !direct && !mentions.iter().any(|mention| mention["mentioned_type"] == "bot") {
        return None;
    }
    let content: Value = serde_json::from_str(message["content"].as_str()?).ok()?;
    let id = message["message_id"].as_str()?;
    let inbound = Inbound::Message {
        id: MessageId(id.into()),
        at: Destination {
            conversation: ConversationId(message["chat_id"].as_str()?.into()),
            // A reply names the root of its thread.
            thread: message["root_id"]
                .as_str()
                .filter(|root| !root.is_empty())
                .map(|root| ThreadId(root.into())),
        },
        sender: UserId(event["sender"]["sender_id"]["open_id"].as_str()?.into()),
        text: readable(content["text"].as_str()?, mentions),
    };
    Some(Addressed {
        id: id.into(),
        inbound,
        direct,
    })
}

/// The text with mentions of bots removed and mentions of people written as `@name`. A
/// mention appears in the text as a key such as `@_user_1`; longer keys are replaced first,
/// so that `@_user_1` does not cut into `@_user_10`.
fn readable(text: &str, mentions: &[Value]) -> String {
    let mut keyed: Vec<(&str, String)> = mentions
        .iter()
        .filter_map(|mention| {
            let key = mention["key"].as_str()?;
            let shown = if mention["mentioned_type"] == "bot" {
                String::new()
            } else {
                format!("@{}", mention["name"].as_str().unwrap_or_default())
            };
            Some((key, shown))
        })
        .collect();
    keyed.sort_by_key(|(key, _)| std::cmp::Reverse(key.len()));
    let mut text = text.to_string();
    for (key, shown) in keyed {
        text = text.replace(key, &shown);
    }
    text.trim().to_string()
}

/// A click on a card button posted by this channel.
fn decision(event: &Value) -> Option<Inbound> {
    let value = &event["action"]["value"];
    let choice = match value["zx"].as_str()? {
        "allow_once" => Choice::AllowOnce,
        "deny" => Choice::Deny,
        _ => return None,
    };
    let chat = event["context"]["open_chat_id"]
        .as_str()
        .or_else(|| value["chat"].as_str())?;
    Some(Inbound::Decision {
        at: Destination {
            conversation: ConversationId(chat.into()),
            thread: value["thread"].as_str().map(|thread| ThreadId(thread.into())),
        },
        user: UserId(event["operator"]["open_id"].as_str()?.into()),
        approval: ApprovalId(value["approval"].as_str()?.into()),
        choice,
    })
}

/// The acknowledgement of a data frame: the frame itself, with the time spent appended as
/// `biz_rt` and a success code as payload (`ws/client_message.go`).
fn acknowledge(frame: &Frame) -> Frame {
    let mut headers = frame.headers.clone();
    // Acknowledged on arrival, before any handling.
    headers.push(("biz_rt".into(), "0".into()));
    Frame {
        seq_id: frame.seq_id,
        log_id: frame.log_id,
        service: frame.service,
        method: frame.method,
        headers,
        payload_encoding: frame.payload_encoding.clone(),
        payload_type: frame.payload_type.clone(),
        payload: ACK_PAYLOAD.to_vec(),
        log_id_new: frame.log_id_new.clone(),
    }
}

/// The `service_id` query parameter of the connection address, which pings carry; 0 when
/// absent.
fn service_id(address: &str) -> i32 {
    address
        .split_once('?')
        .into_iter()
        .flat_map(|(_, query)| query.split('&'))
        .find_map(|pair| pair.strip_prefix("service_id="))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

/// A random wait below `nonce`, from the random bits of a version 4 UUID.
fn jitter(nonce: Duration) -> Duration {
    let millis = u64::try_from(nonce.as_millis()).unwrap_or(u64::MAX);
    if millis == 0 {
        return Duration::ZERO;
    }
    // The low 64 bits of the UUID are random but for two variant bits.
    let random = uuid::Uuid::new_v4().as_u128() as u64;
    Duration::from_millis(random % millis)
}

/// The wait a frequency limit asks for; one second when the header is missing.
fn rate_limit_wait(response: &reqwest::Response) -> Duration {
    let secs = response
        .headers()
        .get(RATE_LIMIT_RESET)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(1);
    Duration::from_secs(secs).min(MAX_INTERVAL)
}

fn split(text: &str) -> Vec<String> {
    super::split(text, MAX_TEXT_CHARS, |out, c| out.push(c))
}

/// The content of a text message: a JSON object as a string.
fn text_content(text: &str) -> String {
    json!({ "text": text }).to_string()
}

/// A card in JSON 2.0 that can be updated, as a string.
fn card(elements: Vec<Value>) -> String {
    json!({
        "schema": "2.0",
        "config": {"update_multi": true},
        "body": {"elements": elements},
    })
    .to_string()
}

fn markdown(content: &str) -> Value {
    json!({"tag": "markdown", "content": content})
}

fn button(label: &str, kind: &str, value: Value) -> Value {
    json!({
        "tag": "button",
        "type": kind,
        "text": {"tag": "plain_text", "content": label},
        "behaviors": [{"type": "callback", "value": value}],
    })
}

/// The result of a closed card: what happened, and who decided.
fn resolution(labels: &CardLabels, message: &Outbound) -> String {
    let Outbound::ApprovalResolved { user, result, .. } = message else {
        return String::new();
    };
    let what = match result {
        ApprovalResult::AllowOnce | ApprovalResult::AllowSession | ApprovalResult::Remembered => &labels.allowed,
        ApprovalResult::Denied => &labels.denied,
        ApprovalResult::TimedOut => &labels.timed_out,
        ApprovalResult::Cancelled => &labels.cancelled,
        ApprovalResult::Failed => &labels.failed,
    };
    match user {
        Some(user) => format!("{what}（{user}）"),
        None => what.clone(),
    }
}

/// Fragments of messages split over several frames, by `message_id`.
#[derive(Default)]
struct Fragments {
    partial: HashMap<String, Partial>,
}

struct Partial {
    slots: Vec<Option<Vec<u8>>>,
    updated: Instant,
}

impl Fragments {
    /// The whole payload once `frame` completes its message; `None` while parts are
    /// missing, or when the frame's fragment headers are unusable.
    fn add(&mut self, frame: &Frame, now: Instant) -> Option<Vec<u8>> {
        let sum: usize = frame.header("sum").and_then(|sum| sum.parse().ok()).unwrap_or(1);
        if sum <= 1 {
            return Some(frame.payload.clone());
        }
        let seq: usize = frame.header("seq")?.parse().ok()?;
        let id = frame.header("message_id")?;
        if seq >= sum || sum > MAX_FRAGMENTS {
            return None;
        }
        self.partial
            .retain(|_, partial| now.duration_since(partial.updated) < FRAGMENT_TTL);
        if !self.partial.contains_key(id) && self.partial.len() >= MAX_PARTIAL {
            return None;
        }
        let partial = self.partial.entry(id.to_string()).or_insert_with(|| Partial {
            slots: vec![None; sum],
            updated: now,
        });
        if partial.slots.len() != sum {
            partial.slots = vec![None; sum];
        }
        partial.slots[seq] = Some(frame.payload.clone());
        partial.updated = now;
        if partial.slots.iter().any(Option::is_none) {
            return None;
        }
        let complete = self.partial.remove(id)?;
        Some(complete.slots.into_iter().flatten().flatten().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fragment(id: &str, sum: usize, seq: usize, payload: &str) -> Frame {
        Frame {
            method: DATA,
            headers: vec![
                ("type".into(), "event".into()),
                ("message_id".into(), id.into()),
                ("sum".into(), sum.to_string()),
                ("seq".into(), seq.to_string()),
            ],
            payload: payload.as_bytes().to_vec(),
            ..Frame::default()
        }
    }

    #[test]
    fn fragments_are_joined_in_seq_order_once_all_arrived() {
        let mut fragments = Fragments::default();
        let now = Instant::now();
        assert_eq!(fragments.add(&fragment("m", 3, 2, "c"), now), None);
        assert_eq!(fragments.add(&fragment("m", 3, 0, "a"), now), None);
        assert_eq!(fragments.add(&fragment("m", 3, 1, "b"), now), Some(b"abc".to_vec()));
        assert!(fragments.partial.is_empty());
    }

    #[test]
    fn a_frame_without_fragments_is_whole() {
        let frame = Frame {
            payload: b"x".to_vec(),
            ..Frame::default()
        };
        assert_eq!(Fragments::default().add(&frame, Instant::now()), Some(b"x".to_vec()));
    }

    #[test]
    fn bad_fragment_headers_and_stale_parts_are_dropped() {
        let mut fragments = Fragments::default();
        let now = Instant::now();
        assert_eq!(fragments.add(&fragment("m", 2, 2, "x"), now), None, "seq beyond sum");
        assert_eq!(fragments.add(&fragment("m", 2, 0, "a"), now), None);
        let later = now + FRAGMENT_TTL + Duration::from_millis(1);
        assert_eq!(
            fragments.add(&fragment("m", 2, 1, "b"), later),
            None,
            "the first part expired"
        );
    }

    #[test]
    fn mentions_of_bots_go_and_people_are_named() {
        let mentions = serde_json::from_value::<Vec<Value>>(json!([
            {"key": "@_user_1", "mentioned_type": "bot", "name": "zx"},
            {"key": "@_user_10", "mentioned_type": "user", "name": "Tom"},
        ]))
        .unwrap();
        assert_eq!(readable("@_user_1 ask @_user_10 now", &mentions), "ask @Tom now");
    }

    #[test]
    fn the_service_id_comes_from_the_address() {
        assert_eq!(service_id("wss://h/ws?device_id=d&service_id=33"), 33);
        assert_eq!(service_id("wss://h/ws?device_id=d"), 0);
    }

    #[test]
    fn an_acknowledgement_echoes_the_frame_with_biz_rt_and_a_success_code() {
        let frame = fragment("m", 1, 0, "{}");
        let ack = acknowledge(&frame);
        assert_eq!(ack.headers[..frame.headers.len()], frame.headers[..]);
        assert_eq!(ack.header("biz_rt"), Some("0"));
        assert_eq!(ack.payload, ACK_PAYLOAD);
        assert_eq!(
            (ack.method, ack.seq_id, ack.log_id),
            (frame.method, frame.seq_id, frame.log_id)
        );
    }

    #[test]
    fn missing_or_zero_settings_keep_the_current_ones() {
        let mut config = ClientConfig::default();
        config.update(&json!({"PingInterval": 0, "ReconnectInterval": 7}));
        assert_eq!(config.ping, DEFAULT_PING_INTERVAL);
        assert_eq!(config.reconnect, Some(Duration::from_secs(7)));
        config.update(&json!({"PingInterval": 1_000_000}));
        assert_eq!(config.ping, MAX_INTERVAL);
    }

    #[test]
    fn jitter_stays_below_the_nonce() {
        assert_eq!(jitter(Duration::ZERO), Duration::ZERO);
        for _ in 0..100 {
            assert!(jitter(Duration::from_millis(5)) < Duration::from_millis(5));
        }
    }
}
