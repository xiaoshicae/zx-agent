//! Slack: messages and button clicks arrive over Socket Mode, posts go through the Web API.
//!
//! Socket Mode needs no public URL: the channel opens a WebSocket to Slack and acknowledges
//! each envelope before handling it.
//!
//! The Web API calls are in `api.rs`; the Socket Mode connection in `connection.rs`; what
//! arrives over it, and the cards, in `events.rs`.

mod api;
mod connection;
mod events;

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use zx_im::{
    ApprovalCard, Backoff, CardLabels, Destination, ErrorReport, ImChannel, ImError, Inbound, MAX_TEXT_CHARS,
    MessageRef, Pacer, Post, RATE_LIMIT_RETRIES, Replacement, SEEN_CAPACITY, Said, Seen, ThreadId, UserId,
};

use api::CallError;
use connection::{Ended, connection_url};
use events::{button, card_request, escape, history, resolution, section, split};

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
/// The reaction that acknowledges a message steered into a run: "seen".
const ACK_REACTION: &str = "eyes";
/// Messages asked for in one `conversations.replies` call: the most Slack recommends
/// (conversations.replies, "Pagination": "We recommend no more than 200 results at a time").
const HISTORY_PAGE_MESSAGES: usize = 200;
/// Pages read of one thread at most, from its start: as many messages as Slack returns of a
/// thread without pagination (conversations.replies, "Pagination by time": "up to 1000
/// messages").
const MAX_HISTORY_PAGES: usize = 5;
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
    /// Called with what the channel goes on after, so that an operator can see it: each
    /// failed attempt to connect, before the channel waits and tries again, a frame it
    /// cannot read, and a thread it could not read to its end.
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
///
/// Optional, each for one more part of the conversation: the `message.channels` and
/// `message.groups` events (scopes `channels:history`, `groups:history`) bring replies in a
/// thread that do not mention the bot, and let it read a thread it is first mentioned in;
/// `users:read` gives senders' names; `reactions:write` acknowledges steering. Without them
/// the bot answers mentions only and shows user IDs.
pub struct SlackChannel {
    config: SlackConfig,
    http: reqwest::Client,
    posts: Pacer,
}

/// What a card shows of the request `card`, open and once closed.
fn shown(labels: &CardLabels, card: &ApprovalCard) -> String {
    card_request(labels, &card.tool, &card.summary, &card.reason)
}

impl SlackChannel {
    /// A channel for `config`. Fails when the HTTP client cannot be built.
    pub fn new(config: SlackConfig) -> Result<Self, ImError> {
        let http = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .build()
            .map_err(|error| ImError::Config(format!("cannot build the HTTP client: {error}")))?;
        Ok(Self {
            posts: Pacer::new(config.post_interval),
            config,
            http,
        })
    }

    /// Tell the operator about `error`, which the channel goes on after.
    fn report(&self, error: &ImError) {
        if let Some(callback) = &self.config.on_error {
            callback(error);
        }
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
            let url = match opened.map(|answer| answer.and_then(|answer| connection_url(&answer))) {
                None => return Ok(()),
                Some(Ok(url)) => Some(url),
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

    async fn send(&self, to: &Destination, message: &Post) -> Result<MessageRef, ImError> {
        match message {
            Post::Text(text) => {
                let (first, rest) = split(text, MAX_TEXT_CHARS);
                let posted = self.post(to, &first, None).await?;
                // The rest go to the same thread, which a first post in the chat itself starts.
                let thread = posted.thread();
                for part in &rest {
                    self.post(&thread, part, None).await?;
                }
                Ok(posted)
            }
            Post::ApprovalRequest { id, card } => {
                let labels = &self.config.labels;
                let request = shown(labels, card);
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
                let fallback = escape(&format!("{}: {}", labels.title, card.tool));
                self.post(to, &fallback, Some(blocks)).await
            }
        }
    }

    async fn update(&self, posted: &MessageRef, replacement: &Replacement) -> Result<(), ImError> {
        match replacement {
            Replacement::Text(text) => {
                let (first, rest) = split(text, MAX_TEXT_CHARS);
                let thread = posted.thread();
                match self.edit(posted, &first, None).await {
                    Ok(()) => {}
                    // Slack refused the edit, as for a message deleted since: the text is
                    // posted instead of lost. A call without an answer may have edited it.
                    Err(CallError::Refused { .. }) => {
                        self.post(&thread, &first, None).await?;
                    }
                    Err(error) => return Err(error.into()),
                }
                for part in &rest {
                    self.post(&thread, part, None).await?;
                }
                Ok(())
            }
            Replacement::ApprovalResolved { card, user, result, .. } => {
                let labels = &self.config.labels;
                let result = resolution(labels, user.as_ref(), *result);
                let blocks = json!([section(&shown(labels, card)), section(&result)]);
                self.edit(posted, &result, Some(blocks)).await.map_err(ImError::from)
            }
        }
    }

    /// `conversations.replies`, page by page to the thread's end (needs `channels:history` or
    /// `groups:history`): Slack returns a thread's root and replies oldest first ("The
    /// earliest messages in the time range are returned first"), so the latest are on the
    /// last page.
    ///
    /// Only the first page fails the call. A later page that cannot be read, or a thread of
    /// more than `MAX_HISTORY_PAGES` pages, ends the reading with the latest of the messages
    /// read so far, and is reported.
    async fn thread_history(&self, at: &Destination, limit: usize) -> Result<Vec<Said>, ImError> {
        let Some(thread) = &at.thread else {
            return Ok(Vec::new());
        };
        let mut latest = Vec::new();
        let mut cursor = String::new();
        for page in 0..MAX_HISTORY_PAGES {
            // A later page is given up at once: waiting out its rate limit could outlast the
            // caller, and the pages read would be lost with it.
            let retries = if page == 0 { RATE_LIMIT_RETRIES } else { 0 };
            let answer = match self.replies_page(at, thread, &cursor, retries).await {
                Ok(answer) => answer,
                Err(error) if page == 0 => return Err(error),
                Err(error) => {
                    self.report(&read_in_part(at, thread, &error.to_string()));
                    return Ok(latest);
                }
            };
            latest.extend(history(&answer));
            // Only the latest are asked for: the older ones go as the pages come.
            let older = latest.len().saturating_sub(limit);
            latest.drain(..older);
            match next_cursor(&answer) {
                Some(next) => cursor = next.to_string(),
                None => return Ok(latest),
            }
        }
        let longer = format!("it has more than {MAX_HISTORY_PAGES} pages of {HISTORY_PAGE_MESSAGES} messages");
        self.report(&read_in_part(at, thread, &longer));
        Ok(latest)
    }

    /// `users.info` (needs `users:read`): the display name, else the real name.
    async fn display_name(&self, user: &UserId) -> Result<Option<String>, ImError> {
        let answer = self
            .call("users.info", &self.config.bot_token, &[("user", user.0.as_str())], None)
            .await?;
        let profile = &answer["user"]["profile"];
        Ok([
            &profile["display_name"],
            &profile["real_name"],
            &answer["user"]["real_name"],
        ]
        .into_iter()
        .filter_map(Value::as_str)
        .find(|name| !name.trim().is_empty())
        .map(String::from))
    }

    /// `reactions.add` with the `eyes` reaction (needs `reactions:write`).
    async fn acknowledge(&self, message: &MessageRef) -> Result<(), ImError> {
        let body = json!({"channel": message.at.conversation.0, "timestamp": message.id.0, "name": ACK_REACTION});
        match self
            .call("reactions.add", &self.config.bot_token, &[], Some(&body))
            .await
        {
            // Acknowledged before, as by a redelivery.
            Err(ImError::Transport(error)) if error.ends_with("already_reacted") => Ok(()),
            other => other.map(|_| ()),
        }
    }
}

/// The cursor of the page after the `conversations.replies` answer `answer`; `None` on the
/// thread's last page, whose cursor is empty or missing.
fn next_cursor(answer: &Value) -> Option<&str> {
    answer["response_metadata"]["next_cursor"]
        .as_str()
        .filter(|cursor| !cursor.is_empty())
}

/// What the operator is told when the thread `thread` of `at`'s chat was not read to its
/// end, and `why`: the messages used are then the latest of those read, not of the thread.
fn read_in_part(at: &Destination, thread: &ThreadId, why: &str) -> ImError {
    ImError::Transport(format!(
        "slack conversations.replies: thread {thread} in {} was not read to its end, so its latest messages may be missing: {why}",
        at.conversation
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

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
