//! Slack: messages and button clicks arrive over Socket Mode, posts go through the Web API.
//!
//! Socket Mode needs no public URL: the channel opens a WebSocket to Slack and acknowledges
//! each envelope before handling it (docs/serve-im-review.md, sections 3.4 and 9.1).

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use reqwest::StatusCode;
use reqwest::header::RETRY_AFTER;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use crate::agent::ApprovalResult;
use crate::sync::lock;

use super::{
    ApprovalId, CardLabels, Choice, ConversationId, Destination, ImChannel, ImError, Inbound, MessageId, MessageRef,
    Outbound, Seen, ThreadId, UserId,
};

/// Where the Web API lives.
pub const DEFAULT_API_BASE: &str = "https://slack.com/api";
const PLATFORM: &str = "slack";
/// `action_id`s of the two card buttons; their `value` is the approval ID.
const ALLOW_ACTION: &str = "zx_allow_once";
const DENY_ACTION: &str = "zx_deny";
/// Messages remembered to drop redeliveries. Slack redelivers an event only for a short
/// while, and this covers far more messages than arrive in that time.
const SEEN_CAPACITY: usize = 1024;
/// Waits between reconnection attempts, doubling up to the maximum.
const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(60);
/// A rate-limited call is retried this many times after waiting as told.
const MAX_RATE_LIMIT_RETRIES: u32 = 3;
/// Characters per posted message; longer text is split. Slack recommends keeping message
/// text within 4,000 characters (chat.postMessage, "Truncating content").
const MAX_TEXT_CHARS: usize = 4000;
/// Characters of a section block's text (Block Kit section block).
const MAX_SECTION_CHARS: usize = 3000;
/// Cards remembered so that closing one keeps the request it showed.
const CARD_CAPACITY: usize = 256;
/// Method errors that retrying cannot fix: the token or the app's setup is wrong.
const SETUP_ERRORS: [&str; 9] = [
    "invalid_auth",
    "missing_args",
    "not_authed",
    "account_inactive",
    "token_revoked",
    "token_expired",
    "not_allowed_token_type",
    "missing_scope",
    "no_permission",
];

/// Settings of a [`SlackChannel`]. The tokens are sent only to `api_base` and never shown.
pub struct SlackConfig {
    /// App-level token (`xapp-`) for Socket Mode.
    pub app_token: String,
    /// Bot token (`xoxb-`) for posting.
    pub bot_token: String,
    /// Web API base URL, [`DEFAULT_API_BASE`] unless a proxy or a test server is used.
    pub api_base: String,
    /// Limit for each Web API call and for opening the WebSocket.
    pub request_timeout: Duration,
    /// Least time between two posts to one chat: Slack generally allows one message per
    /// second per channel (chat.postMessage, "Rate limiting").
    pub post_interval: Duration,
    pub labels: CardLabels,
}

impl SlackConfig {
    pub fn new(app_token: String, bot_token: String) -> Self {
        Self {
            app_token,
            bot_token,
            api_base: DEFAULT_API_BASE.into(),
            request_timeout: Duration::from_secs(30),
            post_interval: Duration::from_secs(1),
            labels: CardLabels::default(),
        }
    }
}

/// Slack through Socket Mode. Needs an app with Socket Mode and Interactivity on, the
/// `app_mention` and `message.im` events, and the scopes to read them and `chat:write`.
pub struct SlackChannel {
    config: SlackConfig,
    http: reqwest::Client,
    /// When each chat may be posted to next. Concurrent sends reserve their turn under the
    /// lock and wait without it.
    next_post: Mutex<HashMap<String, Instant>>,
    /// The request text of open cards, by card message, so that closing a card keeps it.
    cards: Mutex<VecDeque<(MessageId, String)>>,
}

impl SlackChannel {
    pub fn new(config: SlackConfig) -> Result<Self, ImError> {
        let http = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .build()
            .map_err(|error| ImError::Config(format!("cannot build the HTTP client: {error}")))?;
        Ok(Self {
            config,
            http,
            next_post: Mutex::default(),
            cards: Mutex::default(),
        })
    }

    /// Call a Web API method with `token`. A rate limit is waited out and retried; a method
    /// error that retrying cannot fix is a configuration error.
    async fn call(&self, method: &str, token: &str, body: Option<&Value>) -> Result<Value, ImError> {
        let url = format!("{}/{method}", self.config.api_base.trim_end_matches('/'));
        let mut retries = 0;
        loop {
            let request = self.http.post(&url).bearer_auth(token);
            let request = match body {
                Some(body) => request.json(body),
                None => request,
            };
            let response = request
                .send()
                .await
                .map_err(|error| ImError::Transport(format!("slack {method}: {error}")))?;
            let status = response.status();
            if status == StatusCode::TOO_MANY_REQUESTS && retries < MAX_RATE_LIMIT_RETRIES {
                retries += 1;
                tokio::time::sleep(retry_after(&response)).await;
                continue;
            }
            let answer: Value = response
                .json()
                .await
                .map_err(|error| ImError::Transport(format!("slack {method}: HTTP {status}: {error}")))?;
            if answer["ok"] == Value::Bool(true) {
                return Ok(answer);
            }
            let error = answer["error"].as_str().unwrap_or("no error code");
            return Err(if SETUP_ERRORS.contains(&error) {
                ImError::Config(format!("slack {method}: {error}"))
            } else {
                ImError::Transport(format!("slack {method}: {error}"))
            });
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

    async fn post(&self, to: &Destination, text: &str, blocks: Option<Value>) -> Result<MessageRef, ImError> {
        self.turn(&to.conversation.0).await;
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
            .call("chat.postMessage", &self.config.bot_token, Some(&Value::Object(body)))
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
    async fn edit(&self, posted: &MessageRef, text: &str, blocks: Option<Value>) -> Result<(), ImError> {
        let mut body = Map::new();
        body.insert("channel".into(), json!(posted.at.conversation.0));
        body.insert("ts".into(), json!(posted.id.0));
        body.insert("text".into(), json!(text));
        if let Some(blocks) = blocks {
            body.insert("blocks".into(), blocks);
        }
        self.call("chat.update", &self.config.bot_token, Some(&Value::Object(body)))
            .await
            .map(drop)
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

    /// Receive over one Socket Mode connection until it ends.
    async fn receive(
        &self,
        url: &str,
        inbound: &mpsc::Sender<Inbound>,
        bot: Option<&str>,
        seen: &mut Seen<(String, String)>,
        cancel: &CancellationToken,
    ) -> Ended {
        let connected = tokio::time::timeout(self.config.request_timeout, tokio_tungstenite::connect_async(url)).await;
        let mut socket = match connected {
            Ok(Ok((socket, _))) => socket,
            Ok(Err(_)) | Err(_) => return Ended::Failed,
        };
        loop {
            let frame = tokio::select! {
                () = cancel.cancelled() => {
                    // The server is stopping; a failed close changes nothing.
                    socket.close(None).await.ok();
                    return Ended::Cancelled;
                }
                frame = socket.next() => frame,
            };
            let text = match frame {
                Some(Ok(Message::Text(text))) => text,
                // tungstenite answers pings itself; Slack sends no binary frames.
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_))) => continue,
                Some(Ok(Message::Close(_)) | Err(_)) | None => return Ended::Lost,
            };
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            // Acknowledge first: Slack redelivers what is not acknowledged in time.
            if let Some(envelope) = frame["envelope_id"].as_str() {
                let ack = json!({ "envelope_id": envelope }).to_string();
                if socket.send(Message::text(ack)).await.is_err() {
                    return Ended::Lost;
                }
            }
            let events = match frame["type"].as_str() {
                Some("disconnect") => return Ended::Lost,
                Some("events_api") => message(&frame["payload"], bot)
                    .filter(|(key, _)| seen.insert(key))
                    .map(|(_, event)| event)
                    .into_iter()
                    .collect(),
                Some("interactive") => decisions(&frame["payload"]),
                // hello, and envelope types this bot does not subscribe to.
                _ => Vec::new(),
            };
            for event in events {
                if inbound.send(event).await.is_err() {
                    // The server no longer reads: it is stopping.
                    return Ended::Cancelled;
                }
            }
        }
    }
}

enum Ended {
    Cancelled,
    /// Slack asked to reconnect, or the connection dropped.
    Lost,
    /// The WebSocket could not be opened.
    Failed,
}

#[async_trait]
impl ImChannel for SlackChannel {
    fn platform(&self) -> &str {
        PLATFORM
    }

    async fn run(&self, inbound: mpsc::Sender<Inbound>, cancel: &CancellationToken) -> Result<(), ImError> {
        let mut backoff = RECONNECT_MIN;
        // The bot's own user ID tells its messages apart; asking also checks the bot token.
        let bot = loop {
            match self.call("auth.test", &self.config.bot_token, None).await {
                Ok(answer) => break answer["user_id"].as_str().map(String::from),
                Err(error @ ImError::Config(_)) => return Err(error),
                Err(ImError::Transport(_)) => {
                    if !pause(&mut backoff, cancel).await {
                        return Ok(());
                    }
                }
            }
        };
        let mut seen = Seen::new(SEEN_CAPACITY);
        loop {
            if cancel.is_cancelled() {
                return Ok(());
            }
            let url = match self.call("apps.connections.open", &self.config.app_token, None).await {
                Ok(answer) => answer["url"].as_str().map(String::from),
                Err(error @ ImError::Config(_)) => return Err(error),
                Err(ImError::Transport(_)) => None,
            };
            let ended = match url {
                Some(url) => self.receive(&url, &inbound, bot.as_deref(), &mut seen, cancel).await,
                None => Ended::Failed,
            };
            match ended {
                Ended::Cancelled => return Ok(()),
                // Slack refreshes connections every few hours; reconnect at once.
                Ended::Lost => backoff = RECONNECT_MIN,
                Ended::Failed => {
                    if !pause(&mut backoff, cancel).await {
                        return Ok(());
                    }
                }
            }
        }
    }

    async fn send(&self, to: &Destination, message: &Outbound) -> Result<MessageRef, ImError> {
        match message {
            Outbound::Text(text) => {
                let mut parts = split(text, MAX_TEXT_CHARS).into_iter();
                let first = parts.next().unwrap_or_default();
                let posted = self.post(to, &first, None).await?;
                // The rest go to the same thread, which a first post in the chat itself starts.
                let rest_to = Destination {
                    conversation: to.conversation.clone(),
                    thread: Some(to.thread.clone().unwrap_or_else(|| ThreadId(posted.id.0.clone()))),
                };
                for part in parts {
                    self.post(&rest_to, &part, None).await?;
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
                let request = card_request(labels, tool, summary, reason);
                // The thread travels in the button value: the documented block_actions
                // fields do not include the message's thread.
                let value = match &to.thread {
                    Some(thread) => format!("{thread}/{id}"),
                    None => id.0.clone(),
                };
                let blocks = json!([
                    section(&request),
                    {
                        "type": "actions",
                        "elements": [
                            button(&labels.allow_once, ALLOW_ACTION, &value, "primary"),
                            button(&labels.deny, DENY_ACTION, &value, "danger"),
                        ],
                    },
                ]);
                let fallback = escape(&format!("{}：{tool}", labels.title));
                let posted = self.post(to, &fallback, Some(blocks)).await?;
                self.remember_card(&posted.id, request);
                Ok(posted)
            }
            Outbound::ApprovalResolved { .. } => {
                let text = resolution(&self.config.labels, message);
                self.post(to, &text, None).await
            }
        }
    }

    async fn update(&self, posted: &MessageRef, message: &Outbound) -> Result<(), ImError> {
        match message {
            Outbound::Text(text) => {
                let mut parts = split(text, MAX_TEXT_CHARS).into_iter();
                let first = parts.next().unwrap_or_default();
                self.edit(posted, &first, None).await?;
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
                for part in parts {
                    self.post(&thread, &part, None).await?;
                }
                Ok(())
            }
            Outbound::ApprovalRequest { .. } => Err(ImError::Transport(
                "an approval request is posted, not written over a message".into(),
            )),
            Outbound::ApprovalResolved { .. } => {
                let result = resolution(&self.config.labels, message);
                let mut blocks = Vec::new();
                if let Some(request) = self.forget_card(&posted.id) {
                    blocks.push(section(&request));
                }
                blocks.push(section(&result));
                self.edit(posted, &result, Some(Value::Array(blocks))).await
            }
        }
    }
}

/// Wait out the current backoff, doubling it. Returns false when cancelled meanwhile.
async fn pause(backoff: &mut Duration, cancel: &CancellationToken) -> bool {
    let wait = *backoff;
    *backoff = (*backoff * 2).min(RECONNECT_MAX);
    tokio::select! {
        () = cancel.cancelled() => false,
        () = tokio::time::sleep(wait) => true,
    }
}

/// The wait a 429 asks for; one second when the header is missing or unreadable.
fn retry_after(response: &reqwest::Response) -> Duration {
    let secs = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(1);
    Duration::from_secs(secs)
}

/// A message addressed to the bot, with the key that identifies it among redeliveries.
fn message(payload: &Value, bot: Option<&str>) -> Option<((String, String), Inbound)> {
    // Socket Mode payloads are "identical to the typical Events API payloads", which wrap the
    // event; the page's own example shows the bare event, so both are accepted.
    let event = if payload["event"].is_object() {
        &payload["event"]
    } else {
        payload
    };
    let addressed = match event["type"].as_str() {
        Some("app_mention") => true,
        Some("message") => event["channel_type"] == "im",
        _ => false,
    };
    // Edits, deletions and joins carry a subtype; the bot's own posts carry a bot_id.
    if !addressed || event.get("subtype").is_some() || event.get("bot_id").is_some() {
        return None;
    }
    let user = event["user"].as_str()?;
    if Some(user) == bot {
        return None;
    }
    let channel = event["channel"].as_str()?;
    let ts = event["ts"].as_str()?;
    let text = unescape(&without_mention(event["text"].as_str().unwrap_or_default(), bot));
    let inbound = Inbound::Message {
        id: MessageId(ts.into()),
        at: Destination {
            conversation: ConversationId(channel.into()),
            thread: event["thread_ts"].as_str().map(|ts| ThreadId(ts.into())),
        },
        sender: UserId(user.into()),
        text,
    };
    Some(((channel.into(), ts.into()), inbound))
}

/// Clicks on the card buttons of a `block_actions` payload.
fn decisions(payload: &Value) -> Vec<Inbound> {
    if payload["type"] != "block_actions" {
        return Vec::new();
    }
    let (Some(user), Some(channel)) = (
        payload["user"]["id"].as_str(),
        payload["channel"]["id"]
            .as_str()
            .or_else(|| payload["container"]["channel_id"].as_str()),
    ) else {
        return Vec::new();
    };
    // Older cards, or other senders, may leave the thread out of the value.
    let fallback = payload["message"]["thread_ts"]
        .as_str()
        .or_else(|| payload["container"]["thread_ts"].as_str());
    let actions = payload["actions"].as_array().map(Vec::as_slice).unwrap_or_default();
    actions
        .iter()
        .filter_map(|action| {
            let choice = match action["action_id"].as_str()? {
                ALLOW_ACTION => Choice::AllowOnce,
                DENY_ACTION => Choice::Deny,
                _ => return None,
            };
            let value = action["value"].as_str()?;
            let (thread, approval) = match value.split_once('/') {
                Some((thread, approval)) => (Some(thread), approval),
                None => (fallback, value),
            };
            Some(Inbound::Decision {
                at: Destination {
                    conversation: ConversationId(channel.into()),
                    thread: thread.map(|ts| ThreadId(ts.into())),
                },
                user: UserId(user.into()),
                approval: ApprovalId(approval.into()),
                choice,
            })
        })
        .collect()
}

/// The text with mentions of the bot removed.
fn without_mention(text: &str, bot: Option<&str>) -> String {
    match bot {
        Some(bot) => text.replace(&format!("<@{bot}>"), "").trim().to_string(),
        None => text.trim().to_string(),
    }
}

/// Slack writes `&`, `<` and `>` as entities in message text.
fn unescape(text: &str) -> String {
    text.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

/// `&`, `<` and `>` as entities, so that text is never read as a mention or a link.
fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        push_escaped(&mut escaped, c);
    }
    escaped
}

fn push_escaped(out: &mut String, c: char) {
    match c {
        '&' => out.push_str("&amp;"),
        '<' => out.push_str("&lt;"),
        '>' => out.push_str("&gt;"),
        c => out.push(c),
    }
}

/// Escaped text in parts of at most `max` characters, never cut inside an entity.
fn split(text: &str, max: usize) -> Vec<String> {
    super::split(text, max, push_escaped)
}

/// The escaped request shown on a card, within the length of a section block.
fn card_request(labels: &CardLabels, tool: &str, summary: &str, reason: &str) -> String {
    let head = format!("*{}*：`{}`\n```", escape(&labels.title), escape(tool));
    let tail = format!("```\n{}：{}", escape(&labels.reason), escape(reason));
    let room = MAX_SECTION_CHARS.saturating_sub(head.chars().count() + tail.chars().count() + 1);
    let mut summary = split(summary, room).into_iter().next().unwrap_or_default();
    if summary.chars().count() >= room {
        summary.push('…');
    }
    format!("{head}{summary}{tail}")
}

fn section(text: &str) -> Value {
    json!({"type": "section", "text": {"type": "mrkdwn", "text": text}})
}

fn button(label: &str, action: &str, value: &str, style: &str) -> Value {
    json!({
        "type": "button",
        "text": {"type": "plain_text", "text": label},
        "action_id": action,
        "value": value,
        "style": style,
    })
}

/// The escaped result line of a closed card: what happened, and a mention of who decided.
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
    // The mention is markup and stays as it is; only the label is text.
    match user {
        Some(user) => format!("{}（<@{user}>）", escape(what)),
        None => escape(what),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_escaped_and_split_without_breaking_an_entity() {
        assert_eq!(escape("a < b && c > d"), "a &lt; b &amp;&amp; c &gt; d");
        assert_eq!(split("ab<cd", 4), ["ab", "&lt;", "cd"]);
        assert_eq!(split("", 10), [""]);
    }

    #[test]
    fn a_split_prefers_the_last_line_break_of_the_part() {
        assert_eq!(split("aaaa\nbbbb", 7), ["aaaa\n", "bbbb"]);
        // A line break in the first half would leave too short a part; the cut is hard.
        assert_eq!(split("a\nbbbbbbb", 6), ["a\nbbbb", "bbb"]);
    }

    #[test]
    fn a_split_counts_characters_not_bytes() {
        assert_eq!(split("界界界", 2), ["界界", "界"]);
    }

    #[test]
    fn incoming_entities_are_restored() {
        assert_eq!(unescape("a &lt; b &amp;&amp; c &gt; d"), "a < b && c > d");
    }

    #[test]
    fn the_bot_mention_is_removed() {
        assert_eq!(without_mention("<@UBOT> check <@U2>", Some("UBOT")), "check <@U2>");
    }

    #[test]
    fn a_long_request_fits_in_a_section() {
        let request = card_request(&CardLabels::default(), "bash", &"<".repeat(5000), "why");
        assert!(
            request.chars().count() <= MAX_SECTION_CHARS,
            "{}",
            request.chars().count()
        );
        assert!(request.contains("…"));
    }

    #[test]
    fn seen_messages_are_dropped_and_old_ones_forgotten() {
        let mut seen = Seen::new(SEEN_CAPACITY);
        let key = |n: usize| ("C1".to_string(), n.to_string());
        assert!(seen.insert(&key(0)));
        assert!(!seen.insert(&key(0)));
        for n in 1..=SEEN_CAPACITY {
            assert!(seen.insert(&key(n)));
        }
        assert!(seen.insert(&key(0)), "the oldest key is forgotten");
    }
}
