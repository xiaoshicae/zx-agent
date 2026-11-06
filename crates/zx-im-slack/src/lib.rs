//! Slack: messages and button clicks arrive over Socket Mode, posts go through the Web API.
//!
//! Socket Mode needs no public URL: the channel opens a WebSocket to Slack and acknowledges
//! each envelope before handling it (docs/serve-im-review.md, sections 3.4 and 9.1).

use std::time::Duration;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use reqwest::StatusCode;
use reqwest::header::{HeaderMap, RETRY_AFTER};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{Bytes, Message};
use tokio_util::sync::CancellationToken;
use zx_core::agent::ApprovalResult;
use zx_im::{
    ApprovalId, Backoff, CARD_CAPACITY, CardLabels, Cards, Choice, ConversationId, Destination, ErrorReport, ImChannel,
    ImError, Inbound, Keepalive, MAX_TEXT_CHARS, MessageId, MessageRef, Outbound, Pacer, RATE_LIMIT_RETRIES,
    SEEN_CAPACITY, Seen, ThreadId, UserId, card_update_refused, split_text,
};

/// Where the Web API lives.
const DEFAULT_API_BASE: &str = "https://slack.com/api";
const PLATFORM: &str = "slack";
/// Keepalive pings over the Socket Mode connection, by default: often enough to notice a
/// connection that died within half a minute, rare enough to cost nothing.
const DEFAULT_PING_INTERVAL: Duration = Duration::from_secs(10);
/// Longest wait a `Retry-After` is given: Slack's rate limits are per minute (Rate limits,
/// "Tiers"), so a longer value is more likely a bad header than a real wait; a send that
/// waits is dropped by the server's outbound timeout, a connection attempt by the stop.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);
/// `action_id`s of the two card buttons; their `value` is the approval ID.
const ALLOW_ACTION: &str = "zx_allow_once";
const DENY_ACTION: &str = "zx_deny";
/// Characters of a section block's text (Block Kit section block).
const MAX_SECTION_CHARS: usize = 3000;
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
    /// Web API base URL: `https://slack.com/api` unless a proxy or a test server is used.
    pub api_base: String,
    /// Limit for each Web API call and for opening the WebSocket.
    pub request_timeout: Duration,
    /// Least time between two posts to one chat: Slack generally allows one message per
    /// second per channel (chat.postMessage, "Rate limiting").
    pub post_interval: Duration,
    /// Time between two keepalive pings over the Socket Mode connection; a connection that
    /// answers nothing for twice this, plus a margin, is taken as lost and opened again.
    /// Ten seconds by default.
    pub ping_interval: Duration,
    pub labels: CardLabels,
    /// Called with each failed attempt to connect, before the channel waits and tries
    /// again, so that an operator can see a platform that cannot be reached.
    pub on_error: Option<ErrorReport>,
}

impl SlackConfig {
    /// Settings for an app with these tokens, and the defaults of every other field.
    pub fn new(app_token: String, bot_token: String) -> Self {
        Self {
            app_token,
            bot_token,
            api_base: DEFAULT_API_BASE.into(),
            request_timeout: Duration::from_secs(30),
            post_interval: Duration::from_secs(1),
            ping_interval: DEFAULT_PING_INTERVAL,
            labels: CardLabels::default(),
            on_error: None,
        }
    }
}

/// Slack through Socket Mode. Needs an app with Socket Mode and Interactivity on, the
/// `app_mention` and `message.im` events, and the scopes to read them and `chat:write`.
pub struct SlackChannel {
    config: SlackConfig,
    http: reqwest::Client,
    posts: Pacer,
    cards: Cards,
}

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

impl SlackChannel {
    /// A channel for `config`. Fails when the HTTP client cannot be built.
    pub fn new(config: SlackConfig) -> Result<Self, ImError> {
        let http = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .build()
            .map_err(|error| ImError::Config(format!("cannot build the HTTP client: {error}")))?;
        Ok(Self {
            posts: Pacer::new(config.post_interval),
            cards: Cards::new(CARD_CAPACITY),
            config,
            http,
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
            if status == StatusCode::TOO_MANY_REQUESTS && retries < RATE_LIMIT_RETRIES {
                retries += 1;
                tokio::time::sleep(retry_after(response.headers())).await;
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

    /// A Web API call without a body, given up when `cancel` is triggered: `None`.
    async fn call_unless_cancelled(
        &self,
        method: &str,
        token: &str,
        cancel: &CancellationToken,
    ) -> Option<Result<Value, ImError>> {
        tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            answer = self.call(method, token, None) => Some(answer),
        }
    }

    fn report(&self, error: &ImError) {
        if let Some(callback) = &self.config.on_error {
            callback(error);
        }
    }

    async fn post(&self, to: &Destination, text: &str, blocks: Option<Value>) -> Result<MessageRef, ImError> {
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

    /// Open the Socket Mode connection at `url`; a failure is reported.
    async fn open_socket(&self, url: &str) -> Option<Socket> {
        let connected = tokio::time::timeout(self.config.request_timeout, tokio_tungstenite::connect_async(url)).await;
        let failure = match connected {
            Ok(Ok((socket, _))) => return Some(socket),
            Ok(Err(error)) => format!("slack: cannot open the Socket Mode connection: {error}"),
            Err(_) => format!(
                "slack: opening the Socket Mode connection took longer than {} s",
                self.config.request_timeout.as_secs()
            ),
        };
        self.report(&ImError::Transport(failure));
        None
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
        let Some(mut socket) = self.open_socket(url).await else {
            return Ended::Failed;
        };
        let mut keepalive = Keepalive::new(self.config.ping_interval);
        loop {
            let frame = tokio::select! {
                () = cancel.cancelled() => {
                    // The server is stopping; a failed close changes nothing.
                    socket.close(None).await.ok();
                    return Ended::Cancelled;
                }
                () = tokio::time::sleep_until(keepalive.next_ping()) => {
                    if socket.send(Message::Ping(Bytes::new())).await.is_err() {
                        return Ended::Lost;
                    }
                    keepalive.pinged();
                    continue;
                }
                () = tokio::time::sleep_until(keepalive.deadline()) => return Ended::Silent(keepalive.silence()),
                frame = socket.next() => frame,
            };
            keepalive.alive();
            let text = match frame {
                Some(Ok(Message::Text(text))) => text,
                // tungstenite answers pings itself, and the pongs answer ours; Slack sends no
                // binary frames.
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_))) => continue,
                Some(Ok(Message::Close(_)) | Err(_)) | None => return Ended::Lost,
            };
            let frame: Value = match serde_json::from_str(&text) {
                Ok(frame) => frame,
                Err(error) => {
                    self.report(&ImError::Transport(format!(
                        "slack: a Socket Mode frame is not JSON: {error}"
                    )));
                    continue;
                }
            };
            // Acknowledge first: Slack redelivers what is not acknowledged in time.
            if let Some(id) = frame["envelope_id"].as_str() {
                let ack = json!({ "envelope_id": id }).to_string();
                if socket.send(Message::text(ack)).await.is_err() {
                    return Ended::Lost;
                }
            }
            let events = match envelope(&frame, bot, seen) {
                Envelope::Disconnect => return Ended::Lost,
                Envelope::Events(events) => events,
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
    /// Nothing came over the connection for this long, not even answers to the pings.
    Silent(Duration),
    /// The WebSocket could not be opened.
    Failed,
}

/// What a Socket Mode envelope asks for.
enum Envelope {
    /// Slack closes the connection soon; another is to be opened.
    Disconnect,
    /// The messages and clicks to pass on: none for `hello`, and for envelope types this bot
    /// does not subscribe to.
    Events(Vec<Inbound>),
}

/// What `frame` asks for; a redelivered message is left out.
fn envelope(frame: &Value, bot: Option<&str>, seen: &mut Seen<(String, String)>) -> Envelope {
    match frame["type"].as_str() {
        Some("disconnect") => Envelope::Disconnect,
        Some("events_api") => Envelope::Events(
            message(&frame["payload"], bot)
                .filter(|(key, _)| seen.insert(key))
                .map(|(_, event)| event)
                .into_iter()
                .collect(),
        ),
        Some("interactive") => Envelope::Events(decisions(&frame["payload"])),
        _ => Envelope::Events(Vec::new()),
    }
}

#[async_trait]
impl ImChannel for SlackChannel {
    fn platform(&self) -> &str {
        PLATFORM
    }

    async fn run(&self, inbound: mpsc::Sender<Inbound>, cancel: &CancellationToken) -> Result<(), ImError> {
        let mut backoff = Backoff::default();
        // The bot's own user ID tells its messages apart; asking also checks the bot token.
        let bot = loop {
            match self
                .call_unless_cancelled("auth.test", &self.config.bot_token, cancel)
                .await
            {
                None => return Ok(()),
                Some(Ok(answer)) => break answer["user_id"].as_str().map(String::from),
                Some(Err(error @ ImError::Config(_))) => return Err(error),
                Some(Err(error @ ImError::Transport(_))) => {
                    self.report(&error);
                    if !backoff.wait(cancel).await {
                        return Ok(());
                    }
                }
            }
        };
        let mut seen = Seen::new(SEEN_CAPACITY);
        loop {
            let opened = self
                .call_unless_cancelled("apps.connections.open", &self.config.app_token, cancel)
                .await;
            let url = match opened {
                None => return Ok(()),
                Some(Ok(answer)) => match answer["url"].as_str() {
                    Some(url) => Some(url.to_string()),
                    None => {
                        self.report(&ImError::Transport(
                            "slack apps.connections.open: no url in the answer".into(),
                        ));
                        None
                    }
                },
                Some(Err(error @ ImError::Config(_))) => return Err(error),
                Some(Err(error @ ImError::Transport(_))) => {
                    self.report(&error);
                    None
                }
            };
            let ended = match url {
                Some(url) => self.receive(&url, &inbound, bot.as_deref(), &mut seen, cancel).await,
                None => Ended::Failed,
            };
            match ended {
                Ended::Cancelled => return Ok(()),
                // Slack refreshes connections every few hours; reconnect at once.
                Ended::Lost => backoff.reset(),
                Ended::Silent(silence) => {
                    self.report(&ImError::Transport(format!(
                        "slack: the Socket Mode connection was silent for {} s; opening it again",
                        silence.as_secs()
                    )));
                    backoff.reset();
                }
                Ended::Failed => {
                    if !backoff.wait(cancel).await {
                        return Ok(());
                    }
                }
            }
        }
    }

    async fn send(&self, to: &Destination, message: &Outbound) -> Result<MessageRef, ImError> {
        match message {
            Outbound::Text(text) => {
                let (first, rest) = split(text, MAX_TEXT_CHARS);
                let posted = self.post(to, &first, None).await?;
                // The rest go to the same thread, which a first post in the chat itself starts.
                let thread = posted.thread();
                for part in &rest {
                    self.post(&thread, part, None).await?;
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
                let fallback = escape(&format!("{}: {tool}", labels.title));
                let posted = self.post(to, &fallback, Some(blocks)).await?;
                self.cards.remember(&posted.id, request);
                Ok(posted)
            }
            Outbound::ApprovalResolved { user, result, .. } => {
                let text = resolution(&self.config.labels, user.as_ref(), *result);
                self.post(to, &text, None).await
            }
        }
    }

    async fn update(&self, posted: &MessageRef, message: &Outbound) -> Result<(), ImError> {
        match message {
            Outbound::Text(text) => {
                let (first, rest) = split(text, MAX_TEXT_CHARS);
                self.edit(posted, &first, None).await?;
                let thread = posted.thread();
                for part in &rest {
                    self.post(&thread, part, None).await?;
                }
                Ok(())
            }
            Outbound::ApprovalRequest { .. } => Err(card_update_refused()),
            Outbound::ApprovalResolved { user, result, .. } => {
                let result = resolution(&self.config.labels, user.as_ref(), *result);
                let mut blocks = Vec::new();
                if let Some(request) = self.cards.forget(&posted.id) {
                    blocks.push(section(&request));
                }
                blocks.push(section(&result));
                self.edit(posted, &result, Some(Value::Array(blocks))).await
            }
        }
    }
}

/// The wait a 429 asks for in `headers`: one second when `Retry-After` is missing or
/// unreadable, at most [`MAX_RETRY_AFTER`].
fn retry_after(headers: &HeaderMap) -> Duration {
    let secs = headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(1);
    Duration::from_secs(secs).min(MAX_RETRY_AFTER)
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
    // A message without text has nothing for the model: not passed on, like one without a
    // sender.
    let text = unescape(&without_mention(event["text"].as_str()?, bot));
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
    // A payload without actions has nothing to decide.
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

/// Escaped text in parts of at most `max` characters, never cut inside an entity: the first
/// part and the rest.
fn split(text: &str, max: usize) -> (String, Vec<String>) {
    split_text(text, max, push_escaped)
}

/// The escaped request shown on a card, within the length of a section block.
fn card_request(labels: &CardLabels, tool: &str, summary: &str, reason: &str) -> String {
    let head = format!("*{}*: `{}`\n```", escape(&labels.title), escape(tool));
    let tail = format!("```\n{}: {}", escape(&labels.reason), escape(reason));
    let room = MAX_SECTION_CHARS.saturating_sub(head.chars().count() + tail.chars().count() + 1);
    let (mut summary, rest) = split(summary, room);
    // A part ends before the room does when the next entity would not fit, so whether the
    // request was cut shows in the parts left, not in this part's length.
    if !rest.is_empty() {
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
fn resolution(labels: &CardLabels, user: Option<&UserId>, result: ApprovalResult) -> String {
    let what = labels.outcome(result);
    // The mention is markup and stays as it is; only the label is text.
    match user {
        Some(user) => format!("{} (<@{user}>)", escape(what)),
        None => escape(what),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parts of a split in order.
    fn parts((first, rest): (String, Vec<String>)) -> Vec<String> {
        let mut parts = vec![first];
        parts.extend(rest);
        parts
    }

    #[test]
    fn text_is_escaped_and_split_without_breaking_an_entity() {
        assert_eq!(escape("a < b && c > d"), "a &lt; b &amp;&amp; c &gt; d");
        assert_eq!(parts(split("ab<cd", 4)), ["ab", "&lt;", "cd"]);
        assert_eq!(split("", 10), (String::new(), Vec::new()));
    }

    #[test]
    fn a_split_prefers_the_last_line_break_of_the_part() {
        assert_eq!(parts(split("aaaa\nbbbb", 7)), ["aaaa\n", "bbbb"]);
        // A line break in the first half would leave too short a part; the cut is hard.
        assert_eq!(parts(split("a\nbbbbbbb", 6)), ["a\nbbbb", "bbb"]);
    }

    #[test]
    fn a_split_counts_characters_not_bytes() {
        assert_eq!(parts(split("界界界", 2)), ["界界", "界"]);
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
    fn a_cut_request_ends_with_an_ellipsis_when_an_entity_ends_its_part_early() {
        // The room for the request here (2974) is no multiple of the four characters of
        // `&lt;`, so the part that fits ends two characters before the room does.
        let labels = CardLabels {
            title: "T".into(),
            reason: "R".into(),
            ..CardLabels::default()
        };
        let request = card_request(&labels, "bash", &"<".repeat(5000), "why");
        assert!(request.chars().count() <= MAX_SECTION_CHARS);
        assert!(request.contains("&lt;…```"), "{request}");
        assert!(!card_request(&labels, "bash", "ls -l", "why").contains('…'));
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

    #[test]
    fn a_retry_after_is_capped_and_defaults_to_one_second() {
        use reqwest::header::HeaderValue;

        let mut headers = HeaderMap::new();
        assert_eq!(retry_after(&headers), Duration::from_secs(1));
        headers.insert(RETRY_AFTER, HeaderValue::from_static("soon"));
        assert_eq!(retry_after(&headers), Duration::from_secs(1));
        headers.insert(RETRY_AFTER, HeaderValue::from_static("5"));
        assert_eq!(retry_after(&headers), Duration::from_secs(5));
        headers.insert(RETRY_AFTER, HeaderValue::from_static("100000"));
        assert_eq!(retry_after(&headers), MAX_RETRY_AFTER);
    }
}
