//! Messages injected into a running loop, and records an extension asks the loop to keep.

use std::collections::VecDeque;
use std::sync::Mutex;

use serde_json::Value;

use crate::session::CustomMessage;
use crate::sync::lock;

/// A message a frontend or an extension queues for a run (pi: `AgentMessage`).
#[derive(Clone, Debug, PartialEq)]
pub enum QueuedMessage {
    /// Text the person typed. It passes through the `on_input` hooks and is recorded as a
    /// user message.
    User(String),
    /// A message from an extension (pi: `sendMessage`). It is recorded as a `custom_message`
    /// entry, without input hooks, and the model sees it as a user message.
    Custom(CustomMessage),
}

impl From<String> for QueuedMessage {
    fn from(text: String) -> Self {
        Self::User(text)
    }
}

impl From<&str> for QueuedMessage {
    fn from(text: &str) -> Self {
        Self::User(text.to_string())
    }
}

/// How many queued messages a run takes at a time (pi: `QueueMode`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QueueMode {
    /// Every queued message, oldest first, before the next request.
    #[default]
    All,
    /// Only the oldest; the rest wait for later requests, so each gets a turn of its own
    /// (pi's default).
    OneAtATime,
}

impl QueueMode {
    /// The name on the command line: `all` or `one-at-a-time`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::OneAtATime => "one-at-a-time",
        }
    }
}

impl std::str::FromStr for QueueMode {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text.trim() {
            "all" => Ok(Self::All),
            "one-at-a-time" | "one" => Ok(Self::OneAtATime),
            other => Err(format!("unknown queue mode {other:?}; use all or one-at-a-time")),
        }
    }
}

/// A record an extension asked for during a run, kept until the loop can write it.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingEntry {
    pub custom_type: String,
    pub data: Value,
}

/// Messages injected into a running loop, and the custom entries extensions append while a
/// run owns the session.
///
/// - Steering messages are taken after each turn and enter the conversation before the
///   next model request, while the task is still in progress.
/// - Follow-up messages are taken only when the agent would otherwise stop.
/// - Each queue takes all of its messages at once or one at a time ([`QueueMode`]).
/// - Custom entries are recorded at the start of the next turn, or when the run ends.
#[derive(Debug, Default)]
pub struct MessageQueues {
    steering: Mutex<Lane>,
    follow_up: Mutex<Lane>,
    entries: Mutex<Vec<PendingEntry>>,
}

#[derive(Debug, Default)]
struct Lane {
    messages: VecDeque<QueuedMessage>,
    mode: QueueMode,
}

impl MessageQueues {
    /// Queue a user message for the next turn of the current task.
    pub fn steer(&self, text: impl Into<String>) {
        self.steer_message(QueuedMessage::User(text.into()));
    }

    /// Queue a user message for after the current task finishes.
    pub fn follow_up(&self, text: impl Into<String>) {
        self.follow_up_message(QueuedMessage::User(text.into()));
    }

    /// Queue any message for the next turn of the current task.
    pub fn steer_message(&self, message: QueuedMessage) {
        lock(&self.steering).messages.push_back(message);
    }

    /// Queue any message for after the current task finishes.
    pub fn follow_up_message(&self, message: QueuedMessage) {
        lock(&self.follow_up).messages.push_back(message);
    }

    /// Ask the loop to record state of an extension (pi: `appendEntry`) at the start of its
    /// next turn, or when the run ends. Without a run the entry waits for the next one; a
    /// host that is idle records directly through its session instead.
    pub fn append_entry(&self, custom_type: impl Into<String>, data: Value) {
        lock(&self.entries).push(PendingEntry {
            custom_type: custom_type.into(),
            data,
        });
    }

    /// How steering messages are taken.
    pub fn steering_mode(&self) -> QueueMode {
        lock(&self.steering).mode
    }

    /// How follow-up messages are taken.
    pub fn follow_up_mode(&self) -> QueueMode {
        lock(&self.follow_up).mode
    }

    /// Change how steering messages are taken, from the next time the loop looks.
    pub fn set_steering_mode(&self, mode: QueueMode) {
        lock(&self.steering).mode = mode;
    }

    /// Change how follow-up messages are taken, from the next time the loop looks.
    pub fn set_follow_up_mode(&self, mode: QueueMode) {
        lock(&self.follow_up).mode = mode;
    }

    /// Take every queued steering message, oldest first, whatever the mode.
    pub fn drain_steering(&self) -> Vec<QueuedMessage> {
        lock(&self.steering).messages.drain(..).collect()
    }

    /// Take every queued follow-up message, oldest first, whatever the mode.
    pub fn drain_follow_up(&self) -> Vec<QueuedMessage> {
        lock(&self.follow_up).messages.drain(..).collect()
    }

    /// Take every queued message: steering first, then follow-ups, each oldest first.
    pub(crate) fn take_all(&self) -> Vec<QueuedMessage> {
        let mut messages = self.drain_steering();
        messages.extend(self.drain_follow_up());
        messages
    }

    /// Take the entries waiting to be recorded, oldest first.
    pub(crate) fn take_entries(&self) -> Vec<PendingEntry> {
        std::mem::take(&mut *lock(&self.entries))
    }

    /// The oldest message of `queue`, left in place: the loop removes it only once the
    /// session has recorded it, so input that a cancelled or dropped run never recorded is
    /// still queued for the next run.
    pub(crate) fn front(&self, queue: Queue) -> Option<QueuedMessage> {
        lock(self.lane(queue)).messages.front().cloned()
    }

    /// Remove the oldest message of `queue` after the session recorded it, unless a frontend
    /// drained the queue in the meantime and it now starts with another message.
    pub(crate) fn remove_front(&self, queue: Queue, message: &QueuedMessage) {
        let mut lane = lock(self.lane(queue));
        if lane.messages.front() == Some(message) {
            lane.messages.pop_front();
        }
    }

    pub(crate) fn has(&self, queue: Queue) -> bool {
        !lock(self.lane(queue)).messages.is_empty()
    }

    pub(crate) fn mode(&self, queue: Queue) -> QueueMode {
        lock(self.lane(queue)).mode
    }

    fn lane(&self, queue: Queue) -> &Mutex<Lane> {
        match queue {
            Queue::Steering => &self.steering,
            Queue::FollowUp => &self.follow_up,
        }
    }
}

/// One of the two input queues.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Queue {
    Steering,
    FollowUp,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(messages: Vec<QueuedMessage>) -> Vec<String> {
        messages
            .into_iter()
            .map(|message| match message {
                QueuedMessage::User(content) => content,
                other => panic!("expected user messages, got {other:?}"),
            })
            .collect()
    }

    #[test]
    fn draining_takes_messages_oldest_first_and_empties_the_queue() {
        let queues = MessageQueues::default();
        queues.steer("a");
        queues.steer("b");
        queues.follow_up("c");

        assert_eq!(texts(queues.drain_steering()), ["a", "b"]);
        assert!(queues.drain_steering().is_empty());
        assert_eq!(texts(queues.drain_follow_up()), ["c"]);
    }

    #[test]
    fn the_oldest_message_stays_queued_until_removed() {
        let queues = MessageQueues::default();
        queues.steer("a");
        queues.steer("b");

        assert_eq!(queues.front(Queue::Steering), Some("a".into()));
        assert_eq!(queues.front(Queue::Steering), Some("a".into()));
        queues.remove_front(Queue::Steering, &"a".into());
        assert_eq!(queues.front(Queue::Steering), Some("b".into()));
        assert!(!queues.has(Queue::FollowUp));
    }

    #[test]
    fn removing_after_a_drain_leaves_newer_input_queued() {
        let queues = MessageQueues::default();
        queues.follow_up("a");
        // A frontend drains the queue and queues other input while the loop records "a".
        queues.drain_follow_up();
        queues.follow_up("b");

        queues.remove_front(Queue::FollowUp, &"a".into());

        assert_eq!(texts(queues.drain_follow_up()), ["b"]);
    }

    #[test]
    fn custom_messages_and_entries_queue_beside_user_text() {
        let queues = MessageQueues::default();
        let custom = CustomMessage {
            custom_type: "note".into(),
            content: "remember".into(),
            display: false,
            details: None,
        };
        queues.steer("a");
        queues.steer_message(QueuedMessage::Custom(custom.clone()));
        queues.append_entry("state", serde_json::json!({"n": 1}));

        assert_eq!(
            queues.take_all(),
            [QueuedMessage::User("a".into()), QueuedMessage::Custom(custom)]
        );
        assert_eq!(
            queues.take_entries(),
            [PendingEntry {
                custom_type: "state".into(),
                data: serde_json::json!({"n": 1})
            }]
        );
        assert!(queues.take_entries().is_empty());
    }

    #[test]
    fn queue_modes_are_named_and_default_to_all() {
        let queues = MessageQueues::default();
        assert_eq!(queues.steering_mode(), QueueMode::All);
        queues.set_follow_up_mode(QueueMode::OneAtATime);
        assert_eq!(queues.mode(Queue::FollowUp), QueueMode::OneAtATime);
        assert_eq!("one-at-a-time".parse::<QueueMode>(), Ok(QueueMode::OneAtATime));
        assert_eq!("all".parse::<QueueMode>(), Ok(QueueMode::All));
        assert!("some".parse::<QueueMode>().is_err());
        assert_eq!(QueueMode::OneAtATime.as_str(), "one-at-a-time");
    }
}
