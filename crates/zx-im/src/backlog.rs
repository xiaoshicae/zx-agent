//! The backlog: each message a channel receives, kept from its arrival until it is handled,
//! so that a crash, a kill or a stop loses none, and a message delivered again is known for
//! one. The trait [`Backlog`], the server's default [`MemoryBacklog`], and what a message is
//! kept as.
//!
//! - `files`: the backlog of the program `zx-im`, a file per message

mod files;

use std::collections::{HashSet, VecDeque};
use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use zx_util::sync::lock;

use crate::{Addressee, ChatKind, ConversationId, Destination, Inbound, MessageId, ThreadId, UserId};

pub(crate) use files::BacklogFiles;

/// Handled messages a [`MemoryBacklog`] remembers, to know them when they are delivered
/// again: platforms redeliver only for a short while, and this covers far more messages than
/// arrive in that time.
const REMEMBERED: usize = 1024;

/// A message, as the backlog names it: the platform it arrived on, its chat, and its ID
/// there.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MessageKey {
    pub platform: String,
    pub conversation: ConversationId,
    pub message: MessageId,
}

/// Whether a message is new to the backlog ([`Backlog::keep`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kept {
    /// Kept now: the message is to be handled.
    New,
    /// Kept already, or handled lately: the platform delivered it again, and it is not
    /// handled twice.
    Again,
}

/// Keeps each message from its arrival until it is handled
/// ([`ServerConfig::backlog`](crate::ServerConfig::backlog)).
///
/// A channel keeps a message before it acknowledges the delivery
/// ([`Inlet::keep`](crate::Inlet::keep)). The server completes it once it is handled: written
/// into its session, answered without a run, as a command or a refusal is, or dropped on
/// purpose, as `/cancel` drops what was steered. What the server has not handled when it
/// stops stays pending; as it starts, the server handles what is pending, in the order kept,
/// before anything new. The backlog does not read a message's `record`, which is the
/// server's.
#[async_trait]
pub trait Backlog: Send + Sync {
    /// Keep the message `key`, described by `record`. [`Kept::Again`], keeping nothing, when
    /// `key` is pending, or was completed lately: how lately is the backlog's to say, but at
    /// least as long as platforms deliver a message again.
    async fn keep(&self, key: &MessageKey, record: &str) -> Result<Kept, String>;

    /// The message `key` is handled. Completing a message that is not pending succeeds.
    async fn complete(&self, key: &MessageKey) -> Result<(), String>;

    /// The messages pending, with their records, in the order kept. The server calls this
    /// once, before any channel runs.
    async fn pending(&self) -> Result<Vec<(MessageKey, String)>, String>;
}

/// A [`Backlog`] in memory, the server's default: it knows a message delivered again while
/// the process lives, and keeps nothing across a restart, so a crash, a kill or a stop loses
/// what is not handled yet.
pub struct MemoryBacklog {
    kept: Mutex<Memory>,
}

/// What a [`MemoryBacklog`] holds.
struct Memory {
    /// The messages pending, in the order kept. As many as the server's limits let wait.
    pending: Vec<(MessageKey, String)>,
    handled: Seen,
}

impl Default for MemoryBacklog {
    fn default() -> Self {
        Self {
            kept: Mutex::new(Memory {
                pending: Vec::new(),
                handled: Seen::new(REMEMBERED),
            }),
        }
    }
}

#[async_trait]
impl Backlog for MemoryBacklog {
    async fn keep(&self, key: &MessageKey, record: &str) -> Result<Kept, String> {
        let mut kept = lock(&self.kept);
        if kept.handled.contains(key) || kept.pending.iter().any(|(pending, _)| pending == key) {
            return Ok(Kept::Again);
        }
        kept.pending.push((key.clone(), record.to_string()));
        Ok(Kept::New)
    }

    async fn complete(&self, key: &MessageKey) -> Result<(), String> {
        let mut kept = lock(&self.kept);
        kept.pending.retain(|(pending, _)| pending != key);
        kept.handled.insert(key);
        Ok(())
    }

    async fn pending(&self) -> Result<Vec<(MessageKey, String)>, String> {
        Ok(lock(&self.kept).pending.clone())
    }
}

/// Messages handled recently, the oldest forgotten first.
struct Seen {
    order: VecDeque<MessageKey>,
    keys: HashSet<MessageKey>,
    capacity: usize,
}

impl Seen {
    /// Remember the last `capacity` messages.
    fn new(capacity: usize) -> Self {
        Self {
            order: VecDeque::new(),
            keys: HashSet::new(),
            capacity,
        }
    }

    fn contains(&self, key: &MessageKey) -> bool {
        self.keys.contains(key)
    }

    /// Remember `key`, forgetting the oldest beyond the capacity.
    fn insert(&mut self, key: &MessageKey) {
        if !self.keys.insert(key.clone()) {
            return;
        }
        self.order.push_back(key.clone());
        if self.order.len() > self.capacity
            && let Some(oldest) = self.order.pop_front()
        {
            self.keys.remove(&oldest);
        }
    }
}

/// What the backlog keeps of a message besides its key: the rest of [`Inbound::Message`].
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Record {
    /// `None` for a message posted in the chat itself.
    thread: Option<String>,
    chat: ChatKind,
    addressee: Addressee,
    sender: String,
    text: String,
}

/// The key and the record the backlog keeps for `event`, which arrived on `platform`; `None`
/// for a click, which is not kept: approvals end with the process that asked.
pub(crate) fn record(platform: &str, event: &Inbound) -> Option<(MessageKey, String)> {
    match event {
        Inbound::Message {
            id,
            at,
            chat,
            addressee,
            sender,
            text,
        } => {
            let key = MessageKey {
                platform: platform.to_string(),
                conversation: at.conversation.clone(),
                message: id.clone(),
            };
            let record = Record {
                thread: at.thread.as_ref().map(|thread| thread.0.clone()),
                chat: *chat,
                addressee: *addressee,
                sender: sender.0.clone(),
                text: text.clone(),
            };
            let record = serde_json::to_string(&record).expect("strings and unit variants always encode as JSON");
            Some((key, record))
        }
        Inbound::Decision { .. } => None,
    }
}

/// The message `key` that `record` describes, as it arrived; an error for a record that is
/// not one.
pub(crate) fn message(key: &MessageKey, record: &str) -> Result<Inbound, String> {
    let record: Record = serde_json::from_str(record)
        .map_err(|error| format!("the backlog's record of message {} is not one: {error}", key.message))?;
    Ok(Inbound::Message {
        id: key.message.clone(),
        at: Destination {
            conversation: key.conversation.clone(),
            thread: record.thread.map(ThreadId),
        },
        chat: record.chat,
        addressee: record.addressee,
        sender: UserId(record.sender),
        text: record.text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::message_key as key;

    #[tokio::test]
    async fn a_message_is_kept_once_and_known_after_it_is_handled() {
        let backlog = MemoryBacklog::default();
        assert_eq!(backlog.keep(&key("m1"), "one").await, Ok(Kept::New));
        assert_eq!(backlog.keep(&key("m2"), "two").await, Ok(Kept::New));
        assert_eq!(backlog.keep(&key("m1"), "one").await, Ok(Kept::Again));
        assert_eq!(
            backlog.pending().await.unwrap(),
            [(key("m1"), "one".to_string()), (key("m2"), "two".to_string())]
        );
        backlog.complete(&key("m1")).await.unwrap();
        assert_eq!(backlog.pending().await.unwrap(), [(key("m2"), "two".to_string())]);
        assert_eq!(backlog.keep(&key("m1"), "one").await, Ok(Kept::Again));
        // Completing what is not pending changes nothing.
        backlog.complete(&key("m3")).await.unwrap();
        assert_eq!(backlog.pending().await.unwrap().len(), 1);
    }

    #[test]
    fn the_oldest_handled_messages_are_forgotten_first() {
        let mut seen = Seen::new(REMEMBERED);
        seen.insert(&key("0"));
        seen.insert(&key("0"));
        for n in 1..=REMEMBERED {
            seen.insert(&key(&n.to_string()));
        }
        assert!(!seen.contains(&key("0")), "the oldest key is forgotten");
        assert!(seen.contains(&key("1")));
    }

    #[test]
    fn a_message_comes_back_from_its_record_as_it_arrived() {
        let event = Inbound::Message {
            id: MessageId("m1".into()),
            at: Destination {
                conversation: ConversationId("C1".into()),
                thread: Some(ThreadId("t1".into())),
            },
            chat: ChatKind::Group,
            addressee: Addressee::Others,
            sender: UserId("U1".into()),
            text: "多字节 \"quoted\"".into(),
        };
        let (kept, record) = super::record("slack", &event).unwrap();
        assert_eq!(kept, key("m1"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&record).unwrap(),
            serde_json::json!({
                "thread": "t1",
                "chat": "group",
                "addressee": "others",
                "sender": "U1",
                "text": "多字节 \"quoted\"",
            })
        );
        assert_eq!(message(&kept, &record), Ok(event));
        assert!(
            message(&kept, "{}")
                .unwrap_err()
                .contains("record of message m1 is not one")
        );
    }
}
