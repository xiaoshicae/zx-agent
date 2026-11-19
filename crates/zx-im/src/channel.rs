//! What a chat platform does for the server, [`ImChannel`], where it puts what it receives,
//! [`Inlet`], and the words they share: who and where, what arrives and what is posted, the
//! texts of approval cards, and why a call failed.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use zx_agent::events::ApprovalResult;

use crate::backlog::{self, Backlog, Kept};

/// One chat platform, such as Slack.
///
/// The implementation owns the connection and these duties:
/// - it reconnects on its own;
/// - it keeps each message in the server's backlog ([`Inlet::keep`]) before it acknowledges
///   the delivery to the platform, and acknowledges at once, since platforms resend what is
///   not acknowledged within seconds; a message that cannot be kept leaves its delivery
///   unacknowledged, for the platform to deliver again;
/// - only then it passes the delivery's events on ([`Inlet::pass`]), also when the
///   acknowledgement could not be sent, but not a message the backlog has already: one
///   delivered again is known, so the first delivery is its only way to the server;
/// - it passes on direct messages, messages in a group that mention the bot, and replies in
///   a group's threads, each with its [`ChatKind`] and [`Addressee`]; never the bot's own,
///   nor a message posted in a group itself that does not mention the bot. The server
///   decides which to answer;
/// - it queues its sends to stay within the platform's rate limits.
///
/// [`thread_history`](Self::thread_history), [`display_name`](Self::display_name) and
/// [`acknowledge`](Self::acknowledge) have defaults that do nothing, for a platform, or an
/// app without the permissions, that cannot.
#[async_trait]
pub trait ImChannel: Send + Sync {
    /// Name used in configuration and logs, in the labels of the sessions of its
    /// conversations ([`SessionKey::labels`](crate::SessionKey::labels)) and in the names of
    /// the records the server keeps, such as `"slack"`. Unique among the channels of one
    /// server.
    fn platform(&self) -> &str;

    /// Receive from the platform into `inlet` until `cancel`, reconnecting as needed.
    /// Returns `Ok` soon after `cancel`, whatever it is doing, connecting included, once it
    /// has stopped passing events on: the server's stop waits for it. Returns an error that
    /// retrying cannot fix, such as rejected credentials; the server then stops.
    async fn run(&self, inlet: Inlet, cancel: &CancellationToken) -> Result<(), ImError>;

    /// Post `message` to `to`, as a reply in `to.thread` when it is set. Text longer than one
    /// platform message is split into several; the returned reference is the first.
    async fn send(&self, to: &Destination, message: &Post) -> Result<MessageRef, ImError>;

    /// Put `replacement` in the place of the message `posted`, such as the result of an
    /// approval in the place of its card. Text longer than one platform message replaces it
    /// with the first part and posts the rest as replies. Text the platform refuses to put in
    /// place, as for a message deleted since, is posted as a reply instead of lost.
    async fn update(&self, posted: &MessageRef, replacement: &Replacement) -> Result<(), ImError>;

    /// What was said in the thread `at` so far, other bots' messages included, oldest first:
    /// at most the `limit` latest messages, without the bot's own. Read when the bot is first
    /// mentioned in a thread someone else started. The default reads nothing.
    async fn thread_history(&self, _at: &Destination, _limit: usize) -> Result<Vec<Said>, ImError> {
        Ok(Vec::new())
    }

    /// The name people see for `user`, or `None` when the platform gives none. The default
    /// knows no names; the server then shows the user's ID.
    async fn display_name(&self, _user: &UserId) -> Result<Option<String>, ImError> {
        Ok(None)
    }

    /// Show that `message` was taken in, such as with a reaction: a message steered into a
    /// run that is already answering. The default shows nothing.
    async fn acknowledge(&self, _message: &MessageRef) -> Result<(), ImError> {
        Ok(())
    }
}

/// Where a channel puts what it receives ([`ImChannel::run`]): each message is kept in the
/// server's backlog first, then the events go on to the server.
pub struct Inlet {
    platform: String,
    events: mpsc::Sender<Inbound>,
    backlog: Arc<dyn Backlog>,
}

impl Inlet {
    /// Where the channel named `platform` ([`ImChannel::platform`]) puts what it receives:
    /// messages are kept in `backlog`, events passed on to `events`.
    pub fn new(platform: impl Into<String>, events: mpsc::Sender<Inbound>, backlog: Arc<dyn Backlog>) -> Self {
        Self {
            platform: platform.into(),
            events,
            backlog,
        }
    }

    /// Keep `event` in the backlog, before the channel acknowledges its delivery:
    /// [`Kept::New`] for an event to pass on, [`Kept::Again`] for a message the backlog has
    /// already, delivered again, which is not. A click is not kept, and is always new. An error
    /// when the message cannot be kept: the channel then leaves the delivery unacknowledged,
    /// for the platform to deliver it again.
    ///
    /// Not limited in time: a message kept after its time ran out would be known when it is
    /// delivered again, and wait for the next start instead of being handled now.
    pub async fn keep(&self, event: &Inbound) -> Result<Kept, ImError> {
        let Some((key, record)) = backlog::record(&self.platform, event) else {
            return Ok(Kept::New);
        };
        self.backlog
            .keep(&key, &record)
            .await
            .map_err(|error| ImError::Transport(format!("cannot keep message {}: {error}", key.message)))
    }

    /// Pass `event` on to the server, once its delivery is acknowledged; false when the
    /// server no longer reads: it is stopping.
    pub async fn pass(&self, event: Inbound) -> bool {
        self.events.send(event).await.is_ok()
    }
}

macro_rules! id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash)]
        pub struct $name(pub String);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id!(
    /// A chat: a channel, a group or a direct conversation.
    ConversationId
);
id!(
    /// A thread, named by its root message: the replies to that message form the thread.
    ThreadId
);
id!(
    /// A platform user.
    UserId
);
id!(
    /// A message.
    MessageId
);
id!(
    /// One approval request; unique across conversations.
    ApprovalId
);

/// Where a message goes, or where an inbound one was posted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Destination {
    pub conversation: ConversationId,
    /// The root message of the thread; `None` for a message posted in the chat itself.
    pub thread: Option<ThreadId>,
}

/// A posted message, for [`ImChannel::update`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageRef {
    pub at: Destination,
    pub id: MessageId,
}

impl MessageRef {
    /// The thread this message is in, or starts when it was posted in the chat itself: where
    /// the parts after the first of a long text go.
    pub(crate) fn thread(&self) -> Destination {
        Destination {
            conversation: self.at.conversation.clone(),
            thread: Some(self.at.thread.clone().unwrap_or_else(|| ThreadId(self.id.0.clone()))),
        }
    }
}

/// The kind of chat a message was posted in, which sets what one conversation is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChatKind {
    /// One person and the bot: the whole chat is one conversation.
    Direct,
    /// A channel or a group: each thread is a conversation.
    Group,
}

/// Whom a message is meant for, as its mentions tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Addressee {
    /// The bot: the message mentions it, or was posted in a direct chat.
    Bot,
    /// Other people: the message mentions someone, but not the bot.
    Others,
    /// Nobody in particular: the message mentions no one.
    Nobody,
}

/// A message someone posted earlier, as [`ImChannel::thread_history`] reads it.
#[derive(Clone, Debug, PartialEq)]
pub struct Said {
    pub id: MessageId,
    pub sender: UserId,
    pub text: String,
}

/// An event from the platform.
#[derive(Clone, Debug, PartialEq)]
pub enum Inbound {
    /// A message someone posted, a person or another bot; see [`ImChannel`] for which
    /// messages a channel passes on.
    Message {
        id: MessageId,
        at: Destination,
        chat: ChatKind,
        addressee: Addressee,
        sender: UserId,
        text: String,
    },
    /// A click on a button of an approval card. `at` is where the card was posted.
    Decision {
        at: Destination,
        user: UserId,
        approval: ApprovalId,
        choice: Choice,
    },
}

/// What the server posts ([`ImChannel::send`]).
#[derive(Clone, Debug, PartialEq)]
pub enum Post {
    /// Plain text; may contain code blocks.
    Text(String),
    /// A card that shows `card` with an "allow once" and a "deny" button, whose clicks come
    /// back as [`Inbound::Decision`] with this `id`.
    ApprovalRequest { id: ApprovalId, card: ApprovalCard },
}

/// What an approval card shows of the request, open and once closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalCard {
    pub tool: String,
    /// The command, or the call's arguments, shortened for reading at a glance.
    pub summary: String,
    /// Why the policy asks.
    pub reason: String,
}

/// What the server puts in the place of a message it posted ([`ImChannel::update`]).
#[derive(Clone, Debug, PartialEq)]
pub enum Replacement {
    /// Plain text; may contain code blocks. The answer of a run, in the place of its status.
    Text(String),
    /// How an approval ended, in the place of its card, which shows `card` again; `user` is
    /// who clicked, `None` when nobody decided (timeout, cancellation, failure).
    ApprovalResolved {
        id: ApprovalId,
        card: ApprovalCard,
        user: Option<UserId>,
        result: ApprovalResult,
    },
}

/// A button of an approval card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    AllowOnce,
    Deny,
}

/// Texts of approval cards, for every platform. The defaults are English, like
/// [`Replies`](crate::Replies); a deployment sets its users' language, for example in im.toml,
/// the configuration of the program `zx-im` (`[cardLabels]`, fields in camelCase, each
/// optional).
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct CardLabels {
    /// Heads a card; followed by the tool's name.
    pub title: String,
    /// Followed by the reason the policy gave.
    pub reason: String,
    pub allow_once: String,
    pub deny: String,
    /// Replace the buttons once decided; followed by who decided, in parentheses.
    pub allowed: String,
    pub denied: String,
    pub timed_out: String,
    pub cancelled: String,
}

impl Default for CardLabels {
    fn default() -> Self {
        Self {
            title: "Approval needed".into(),
            reason: "Reason".into(),
            allow_once: "Allow once".into(),
            deny: "Deny".into(),
            allowed: "Allowed".into(),
            denied: "Denied".into(),
            timed_out: "Nobody decided in time; not run".into(),
            cancelled: "Cancelled; not run".into(),
        }
    }
}

impl CardLabels {
    /// The label of a closed card: what happened to the call.
    pub(crate) fn outcome(&self, result: ApprovalResult) -> &str {
        match result {
            ApprovalResult::AllowOnce | ApprovalResult::AllowSession | ApprovalResult::Remembered => &self.allowed,
            ApprovalResult::Denied => &self.denied,
            ApprovalResult::TimedOut => &self.timed_out,
            ApprovalResult::Cancelled => &self.cancelled,
        }
    }
}

/// A callback that a channel calls with what it goes on after, for an operator to see: each
/// failed attempt to reach its platform, and what it gave up on without failing a call.
pub type ErrorReport = Arc<dyn Fn(&ImError) + Send + Sync>;

/// Why a channel call or the server failed. The server returns the error of a channel that
/// fails, and its own: settings it refuses, such as two channels with one platform name,
/// sessions it cannot read as it starts, and a channel that stops while it serves.
#[derive(Debug, thiserror::Error)]
pub enum ImError {
    /// Settings or credentials that the platform or the server rejects; retrying does not
    /// help.
    #[error("configuration error: {0}")]
    Config(String),
    /// The platform could not be reached or refused the call, or a channel stopped; a later
    /// attempt may succeed.
    #[error("{0}")]
    Transport(String),
}
