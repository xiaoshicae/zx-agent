//! Chat platforms: the [`ImChannel`] extension point, and a [`Server`] that answers each
//! thread with its own [`AgentSession`]. Design: docs/serve-im-review.md.
//!
//! A channel only moves messages. The server owns routing, sessions, the run limit and
//! approvals, so supporting another platform means implementing the four methods of
//! [`ImChannel`], in a crate of its own; [`split_text`] and [`Seen`] help with two of the
//! duties. The platforms of this workspace are such crates: `zx-im-slack` and
//! `zx-im-feishu`.
//!
//! The `zx-agent` crate re-exports this crate as `zx_agent::im`, with those platforms as
//! `zx_agent::im::slack` and `zx_agent::im::feishu`.

mod approval;
mod server;
mod status;
mod worker;

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::hash::Hash;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
// The modules name the model layer, the kernel and the shared helpers by these crate-root paths.
use zx_ai as ai;
use zx_core::{AgentSession, agent, agent_session, interaction, runner};

use crate::agent::ApprovalResult;

pub use server::{ApprovalRecord, Replies, Server, ServerConfig, ServerEvent, ServerSink, SessionFactory, SessionKey};
pub use status::{StatusFiles, StatusStore};

/// One chat platform, such as Slack.
///
/// The implementation owns the connection and these duties:
/// - it reconnects on its own;
/// - it acknowledges each delivery to the platform at once and only then hands the event
///   over, since platforms resend what is not acknowledged within seconds;
/// - it passes each message on at most once, deduplicating redeliveries;
/// - it passes on only messages addressed to the bot (mentions in a chat, direct messages),
///   never the bot's own;
/// - it queues its sends to stay within the platform's rate limits.
#[async_trait]
pub trait ImChannel: Send + Sync {
    /// Name used in configuration, logs and session paths, such as `"slack"`. Unique among
    /// the channels of one server.
    fn platform(&self) -> &str;

    /// Receive from the platform into `inbound` until `cancel`, reconnecting as needed.
    /// Returns `Ok` once cancelled, after it has stopped sending, or an error that retrying
    /// cannot fix, such as rejected credentials; the server then stops.
    async fn run(&self, inbound: mpsc::Sender<Inbound>, cancel: &CancellationToken) -> Result<(), ImError>;

    /// Post `message` to `to`, as a reply in `to.thread` when it is set. Text longer than one
    /// platform message is split into several; the returned reference is the first.
    async fn send(&self, to: &Destination, message: &Outbound) -> Result<MessageRef, ImError>;

    /// Replace a posted message, such as an approval card once decided. Text longer than one
    /// platform message replaces it with the first part and posts the rest as replies.
    async fn update(&self, posted: &MessageRef, message: &Outbound) -> Result<(), ImError>;
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

/// An event from the platform.
#[derive(Clone, Debug, PartialEq)]
pub enum Inbound {
    /// A message addressed to the bot: a mention in a chat, or a direct message.
    Message {
        id: MessageId,
        at: Destination,
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

/// What the server posts.
#[derive(Clone, Debug, PartialEq)]
pub enum Outbound {
    /// Plain text; may contain code blocks.
    Text(String),
    /// A card with an "allow once" and a "deny" button, whose clicks come back as
    /// [`Inbound::Decision`] with this `id`.
    ApprovalRequest {
        id: ApprovalId,
        tool: String,
        /// The command, or the call's arguments, shortened for reading at a glance.
        summary: String,
        /// Why the policy asks.
        reason: String,
    },
    /// Replaces the card once the approval ended; `user` is who clicked, `None` when nobody
    /// decided (timeout, cancellation, failure).
    ApprovalResolved {
        id: ApprovalId,
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

/// Texts of approval cards, for every platform. The defaults are English, like [`Replies`];
/// a deployment sets its users' language, for example in the configuration of `serve`
/// (`[cardLabels]`, fields in camelCase, each optional).
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
    pub failed: String,
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
            failed: "Approval failed; not run".into(),
        }
    }
}

/// Text in parts of at most `max` characters, each written by `push`, which may write one
/// character as several (an entity, for example) but never splits what it writes. A part
/// ends at its last line break when that keeps at least half of it. Empty text is one empty
/// part. For a channel that must split text longer than one platform message.
pub fn split_text(text: &str, max: usize, push: fn(&mut String, char)) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut count = 0;
    for c in text.chars() {
        let mut piece = String::new();
        push(&mut piece, c);
        let size = piece.chars().count();
        if count + size > max && count > 0 {
            let cut = current
                .rfind('\n')
                .filter(|at| current[..*at].chars().count() >= max / 2);
            match cut {
                Some(at) => {
                    let rest = current.split_off(at + 1);
                    parts.push(std::mem::replace(&mut current, rest));
                    count = current.chars().count();
                }
                None => {
                    parts.push(std::mem::take(&mut current));
                    count = 0;
                }
            }
        }
        current.push_str(&piece);
        count += size;
    }
    parts.push(current);
    parts
}

/// Messages a channel has passed on recently, to drop redeliveries; the oldest are
/// forgotten first.
pub struct Seen<K> {
    order: VecDeque<K>,
    keys: HashSet<K>,
    capacity: usize,
}

impl<K: Clone + Eq + Hash> Seen<K> {
    /// Remember the last `capacity` keys.
    pub fn new(capacity: usize) -> Self {
        Self {
            order: VecDeque::new(),
            keys: HashSet::new(),
            capacity,
        }
    }

    /// Record `key`; false if it was seen already.
    pub fn insert(&mut self, key: &K) -> bool {
        if !self.keys.insert(key.clone()) {
            return false;
        }
        self.order.push_back(key.clone());
        if self.order.len() > self.capacity
            && let Some(oldest) = self.order.pop_front()
        {
            self.keys.remove(&oldest);
        }
        true
    }
}

/// A callback that a channel calls with each failed attempt to reach its platform.
pub type ErrorReport = Arc<dyn Fn(&ImError) + Send + Sync>;

/// Why a channel call failed.
#[derive(Debug)]
pub enum ImError {
    /// Settings or credentials the platform rejects; retrying does not help.
    Config(String),
    /// The platform could not be reached or refused the call; a later call may succeed.
    Transport(String),
}

impl fmt::Display for ImError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(message) => write!(f, "configuration error: {message}"),
            Self::Transport(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ImError {}
