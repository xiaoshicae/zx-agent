//! Slack: messages and button clicks arrive over Socket Mode, posts go through the Web API.
//!
//! Socket Mode needs no public URL: the channel opens a WebSocket to Slack and acknowledges
//! each envelope once its message is kept, before handling it.
//!
//! - `api`: the Web API calls
//! - `connection`: the Socket Mode connection
//! - `events`: what arrives over it, and the text and blocks the channel posts

mod api;
mod connection;
mod events;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Url;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use self::api::CallError;
use self::events::{Me, button, card_request, escape, history, resolution, section, split};
use super::{
    Backoff, BuiltChannel, Connection, DEFAULT_REQUEST_TIMEOUT, Ended, MAX_TEXT_CHARS, Pacer, PlatformFactory,
    PlatformInput, RATE_LIMIT_RETRIES, SLACK, body_within, content_type_is, http_client, report, secret,
};
use crate::{
    CardLabels, Destination, ErrorReport, ImChannel, ImError, ImageRef, Inlet, MessageRef, Post, Replacement, Said,
    ThreadId, UserId,
};

/// Where the Web API lives.
const DEFAULT_API_BASE: &str = "https://slack.com/api";
/// Where Slack keeps the files of messages, which its private URLs name.
const FILE_HOST: &str = "files.slack.com";
/// The variables that hold the tokens, unless the `[slack]` section of im.toml names others.
const DEFAULT_APP_TOKEN_ENV: &str = "SLACK_APP_TOKEN";
const DEFAULT_BOT_TOKEN_ENV: &str = "SLACK_BOT_TOKEN";
/// Keepalive pings over the Socket Mode connection, by default: often enough to notice a
/// connection that died within half a minute, rare enough to cost nothing.
const DEFAULT_PING_INTERVAL: Duration = Duration::from_secs(10);
/// Least time between two posts to one chat, by default: see [`SlackConfig::post_interval`].
const DEFAULT_POST_INTERVAL: Duration = Duration::from_secs(1);
/// `action_id`s of the two card buttons; their `value` is the approval ID.
const ALLOW_ACTION: &str = "zx_allow_once";
const DENY_ACTION: &str = "zx_deny";
/// Characters of a section block's text (Block Kit section block).
const MAX_SECTION_CHARS: usize = 3000;
/// The reaction that acknowledges a message steered into a run: "seen".
const ACK_REACTION: &str = "eyes";
/// Messages asked for in one `conversations.replies` or `conversations.history` call: the
/// most Slack recommends (both methods, "Pagination": "We recommend no more than 200 results
/// at a time").
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

/// Settings of a [`SlackChannel`]. The tokens are sent only to `api_base`, the bot token
/// also to Slack's file host for the images of messages, and never shown.
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
    /// failed attempt to connect, before the channel waits and tries again, a connection
    /// that went silent, a frame it cannot read, and a thread it could not read to its end.
    pub on_error: Option<ErrorReport>,
}

impl SlackConfig {
    /// Settings for an app with these tokens, and the defaults of every other field.
    pub fn new(app_token: String, bot_token: String) -> Self {
        Self {
            app_token,
            bot_token,
            api_base: DEFAULT_API_BASE.into(),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            post_interval: DEFAULT_POST_INTERVAL,
            ping_interval: DEFAULT_PING_INTERVAL,
            labels: CardLabels::default(),
            on_error: None,
        }
    }
}

/// The `[slack]` section of im.toml: which variables hold the tokens, and where the Web API is.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SlackSection {
    /// Variable holding the app-level token (`xapp-`), used for Socket Mode.
    #[serde(default = "default_app_token_env")]
    app_token_env: String,
    /// Variable holding the bot token (`xoxb-`), used to post.
    #[serde(default = "default_bot_token_env")]
    bot_token_env: String,
    /// Web API base URL, for a proxy or a test server.
    api_base: Option<String>,
}

fn default_app_token_env() -> String {
    DEFAULT_APP_TOKEN_ENV.into()
}

fn default_bot_token_env() -> String {
    DEFAULT_BOT_TOKEN_ENV.into()
}

/// Slack, configured by the `[slack]` section of im.toml.
pub(super) struct Slack;

impl PlatformFactory for Slack {
    /// The variables holding the tokens are the ones commands must not see. Fails when a
    /// variable does not hold its token, and when the channel cannot be built.
    fn build(&self, input: PlatformInput<'_>) -> anyhow::Result<BuiltChannel> {
        let section: SlackSection = toml::Value::Table(input.section.clone()).try_into()?;
        let mut config = SlackConfig::new(
            secret(&section.app_token_env, "the Slack app-level token")?,
            secret(&section.bot_token_env, "the Slack bot token")?,
        );
        if let Some(base) = section.api_base {
            config.api_base = base;
        }
        config.labels.clone_from(input.labels);
        config.on_error = Some(input.on_error);
        let channel = SlackChannel::new(config)?;
        Ok(BuiltChannel {
            channel: Arc::new(channel),
            hidden_env: vec![section.app_token_env, section.bot_token_env],
        })
    }
}

/// Slack through Socket Mode. Needs an app with Socket Mode and Interactivity on, the
/// `app_mention` and `message.im` events, and the scopes to read them and `chat:write`.
///
/// Optional, each for one more part of the conversation: the `message.channels` and
/// `message.groups` events (scopes `channels:history`, `groups:history`) bring replies in a
/// thread that do not mention the bot, and let it read a thread it is first mentioned in and
/// a channel since it was last mentioned there;
/// `users:read` gives senders' names; `reactions:write` acknowledges steering. Without them
/// the bot answers mentions only and shows user IDs.
///
/// What other bots post in a thread is heard like anyone's talk there, an alert that someone
/// asks about for one, and read with the thread; it is never answered, and starts nothing.
pub struct SlackChannel {
    config: SlackConfig,
    http: reqwest::Client,
    posts: Pacer,
    /// Who the bot is, once `run` has asked: a thread read before that leaves every bot out.
    me: OnceLock<Me>,
}

impl SlackChannel {
    /// A channel for `config`. Fails when the HTTP client cannot be built.
    pub fn new(config: SlackConfig) -> Result<Self, ImError> {
        Ok(Self {
            http: http_client(config.request_timeout)?,
            posts: Pacer::new(config.post_interval),
            config,
            me: OnceLock::new(),
        })
    }
}

#[async_trait]
impl ImChannel for SlackChannel {
    fn platform(&self) -> &str {
        SLACK
    }

    async fn run(&self, inlet: Inlet, cancel: &CancellationToken) -> Result<(), ImError> {
        let mut backoff = Backoff::default();
        // The bot's own IDs tell its messages apart; asking also checks the bot token.
        let me = loop {
            match self
                .call_unless_cancelled("auth.test", &self.config.bot_token, cancel)
                .await
            {
                None => return Ok(()),
                Some(Ok(answer)) => {
                    let id = |field: &str| answer[field].as_str().map(String::from);
                    break Me {
                        user: id("user_id"),
                        bot: id("bot_id"),
                    };
                }
                Some(Err(error @ ImError::Config(_))) => return Err(error),
                Some(Err(error @ ImError::Transport(_))) => {
                    report(self.config.on_error.as_ref(), &error);
                    if !backoff.wait(cancel).await {
                        return Ok(());
                    }
                }
            }
        };
        let me = self.me.get_or_init(|| me);
        loop {
            let connected = tokio::select! {
                biased;
                () = cancel.cancelled() => return Ok(()),
                connected = self.connect() => connected?,
            };
            let socket = match connected {
                Connection::Open(socket) => socket,
                Connection::Later(reason) => {
                    report(self.config.on_error.as_ref(), &ImError::Transport(reason));
                    if !backoff.wait(cancel).await {
                        return Ok(());
                    }
                    continue;
                }
            };
            let why = match self.receive(*socket, &inlet, me, cancel).await {
                Ended::Cancelled => return Ok(()),
                // Slack refreshes connections every few hours; reconnect at once.
                Ended::Closed => None,
                Ended::Lost(why) => Some(format!("was lost ({why})")),
                Ended::Silent(silence) => Some(format!("was silent for {silence:?}")),
            };
            if let Some(why) = why {
                let reconnecting = format!("slack: the Socket Mode connection {why}; opening it again");
                report(self.config.on_error.as_ref(), &ImError::Transport(reconnecting));
            }
            backoff.reset();
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
                let request = card_request(labels, card);
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
                let blocks = json!([section(&card_request(labels, card)), section(&result)]);
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
        let unknown = Me::default();
        let me = self.me.get().unwrap_or(&unknown);
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
                    report(
                        self.config.on_error.as_ref(),
                        &read_in_part(at, thread, &error.to_string()),
                    );
                    return Ok(latest);
                }
            };
            latest.extend(history(&answer, me));
            // Only the latest are asked for: the older ones go as the pages come.
            let older = latest.len().saturating_sub(limit);
            latest.drain(..older);
            match next_cursor(&answer) {
                Some(next) => cursor = next.to_string(),
                None => return Ok(latest),
            }
        }
        let longer = format!("it has more than {MAX_HISTORY_PAGES} pages of {HISTORY_PAGE_MESSAGES} messages");
        report(self.config.on_error.as_ref(), &read_in_part(at, thread, &longer));
        Ok(latest)
    }

    /// `conversations.history` up to `mention` (needs `channels:history` or
    /// `groups:history`): one page, which Slack returns the latest first ("The most recent
    /// messages in the time range are returned first"), and which holds `limit` messages
    /// unless most of it is left out, such as joins. The method leaves threads to
    /// `conversations.replies` ("To retrieve a message from a thread, check out
    /// conversations.replies").
    async fn chat_history(&self, mention: &MessageRef, limit: usize) -> Result<Vec<Said>, ImError> {
        let unknown = Me::default();
        let me = self.me.get().unwrap_or(&unknown);
        let answer = self.history_page(mention).await?;
        let mut latest = history(&answer, me);
        latest.truncate(limit);
        latest.reverse();
        Ok(latest)
    }

    /// `users.info` (needs `users:read`): the display name, else the real name. An
    /// integration that posts without a user is known by its bot ID, which `bots.info` names
    /// (the same scope).
    async fn display_name(&self, user: &UserId) -> Result<Option<String>, ImError> {
        let token = &self.config.bot_token;
        let answer = match self.call("users.info", token, &[("user", user.0.as_str())], None).await {
            Err(CallError::Refused { code, .. }) if code == "user_not_found" => {
                let answer = self.call("bots.info", token, &[("bot", user.0.as_str())], None).await?;
                return Ok(answer["bot"]["name"].as_str().map(String::from));
            }
            answer => answer?,
        };
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
            Err(CallError::Refused { code, .. }) if code == "already_reacted" => Ok(()),
            other => other.map(drop).map_err(ImError::from),
        }
    }

    /// The file behind `image`'s private URL, fetched with the bot token (needs `files:read`).
    async fn image(&self, _message: &MessageRef, image: &ImageRef, max_bytes: usize) -> Result<Vec<u8>, ImError> {
        let failed = |why: String| ImError::Transport(format!("slack file {}: {why}", image.name));
        let url = self.file_url(&image.key)?;
        let response = self
            .http
            .get(url)
            .bearer_auth(&self.config.bot_token)
            .send()
            .await
            .map_err(|error| failed(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(failed(format!("HTTP {status}")));
        }
        // Slack answers a token it does not take with its sign-in page, under status 200.
        if content_type_is(&response, "text/html") {
            return Err(failed(
                "Slack sent a page instead of the file; the bot token may lack files:read".into(),
            ));
        }
        body_within(response, max_bytes).await.map_err(failed)
    }
}

impl SlackChannel {
    /// `key`, a file's private URL, as a URL the bot token may go to: on Slack's file host, or
    /// on the host of `api_base`, such as a test server. A file elsewhere, such as a remote
    /// file another app shared, is refused unfetched: its URL is not Slack's.
    fn file_url(&self, key: &str) -> Result<Url, ImError> {
        let refused = || ImError::Transport(format!("slack: {key} is not a URL of a file Slack keeps"));
        let url = Url::parse(key).map_err(|_| refused())?;
        let api = Url::parse(&self.config.api_base)
            .map_err(|error| ImError::Config(format!("slack: invalid API base {}: {error}", self.config.api_base)))?;
        let on_api_host = url.scheme() == api.scheme()
            && url.host_str() == api.host_str()
            && url.port_or_known_default() == api.port_or_known_default();
        let on_file_host = url.scheme() == "https" && url.host_str() == Some(FILE_HOST);
        if on_api_host || on_file_host {
            Ok(url)
        } else {
            Err(refused())
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
    fn a_slack_section_defaults_its_variables() {
        let section: SlackSection = toml::from_str("").unwrap();
        assert_eq!(
            (section.app_token_env.as_str(), section.bot_token_env.as_str()),
            (DEFAULT_APP_TOKEN_ENV, DEFAULT_BOT_TOKEN_ENV)
        );
    }
}
