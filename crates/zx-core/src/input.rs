//! Messages injected into a running loop, and records an extension asks the loop to keep.

use std::collections::VecDeque;
use std::sync::Mutex;

use serde_json::Value;

use crate::sync::lock;
use crate::transcript::CustomMessage;

/// A message a frontend or an extension queues for a run (pi: `AgentMessage`).
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    /// Text a person sent. It passes through the `on_input` hooks and is recorded as a user
    /// message. A string converts into one without an actor.
    User {
        text: String,
        /// Who sent it, as the host names the people it serves, such as a user ID of a chat
        /// where several people talk to one session; `None` when the host does not say. The
        /// kernel records it with the message and tells the hooks and the policies
        /// ([`HookContext::actor`](crate::hooks::HookContext::actor),
        /// [`PolicyContext::actor`](crate::policy::PolicyContext::actor)); it decides
        /// nothing by it, and the model does not see it.
        actor: Option<String>,
    },
    /// A message from an extension (pi: `sendMessage`). It is recorded as a `custom_message`
    /// entry, without input hooks, and the model sees it as a user message.
    Custom(CustomMessage),
}

impl From<String> for Input {
    fn from(text: String) -> Self {
        Self::User { text, actor: None }
    }
}

impl From<&str> for Input {
    fn from(text: &str) -> Self {
        Self::from(text.to_string())
    }
}

/// How many queued messages a run takes at a time, from either queue (pi: `QueueMode`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QueueMode {
    /// Every queued message, oldest first, before the next request.
    #[default]
    All,
    /// Only the oldest; the rest wait for later requests, so each gets a step of its own
    /// (pi's default).
    OneAtATime,
}

/// A record an extension asked for during a run, kept until the loop can write it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PendingEntry {
    pub(crate) custom_type: String,
    pub(crate) data: Value,
}

/// Messages injected into a running loop, and the custom entries extensions append while a
/// run owns the session.
///
/// - Steering messages are taken after each step and enter the conversation before the
///   next model request, while the task is still in progress.
/// - Follow-up messages are taken only when the agent would otherwise stop.
/// - Both queues take all of their messages at once or one at a time ([`QueueMode`]).
/// - Custom entries are recorded at the start of the next step, or when the run ends.
#[derive(Debug, Default)]
pub struct InputQueues {
    steering: Mutex<VecDeque<Input>>,
    follow_up: Mutex<VecDeque<Input>>,
    mode: Mutex<QueueMode>,
    entries: Mutex<Vec<PendingEntry>>,
}

impl InputQueues {
    /// Queue a user message for the next step of the current task.
    pub fn steer(&self, text: impl Into<String>) {
        self.steer_message(Input::from(text.into()));
    }

    /// Queue a user message for after the current task finishes.
    pub fn follow_up(&self, text: impl Into<String>) {
        self.follow_up_message(Input::from(text.into()));
    }

    /// Queue any message for the next step of the current task.
    pub fn steer_message(&self, message: Input) {
        lock(&self.steering).push_back(message);
    }

    /// Queue any message for after the current task finishes.
    pub fn follow_up_message(&self, message: Input) {
        lock(&self.follow_up).push_back(message);
    }

    /// Ask the loop to record state of an extension (pi: `appendEntry`) at the start of its
    /// next step, or when the run ends. Without a run the entry waits for the next one; a
    /// host that is idle records directly through its session instead.
    pub fn append_entry(&self, custom_type: impl Into<String>, data: Value) {
        lock(&self.entries).push(PendingEntry {
            custom_type: custom_type.into(),
            data,
        });
    }

    /// How queued messages are taken.
    pub fn mode(&self) -> QueueMode {
        *lock(&self.mode)
    }

    /// Change how queued messages are taken, from the next time the loop looks.
    pub fn set_mode(&self, mode: QueueMode) {
        *lock(&self.mode) = mode;
    }

    /// Withdraw the oldest queued message equal to `message`, from the steering queue if it
    /// has one, else from the follow-up queue (pi: a queued submission's `abort`). Returns
    /// whether one was queued. Works during a run too; a message the loop has already taken
    /// is recorded even so, since it leaves the queue only once recorded.
    pub fn withdraw(&self, message: &Input) -> bool {
        [Queue::Steering, Queue::FollowUp].into_iter().any(|queue| {
            let mut messages = lock(self.lane(queue));
            match messages.iter().position(|queued| queued == message) {
                Some(index) => {
                    messages.remove(index);
                    true
                }
                None => false,
            }
        })
    }

    /// Take every queued steering message, oldest first, whatever the mode.
    pub fn drain_steering(&self) -> Vec<Input> {
        lock(&self.steering).drain(..).collect()
    }

    /// Take every queued follow-up message, oldest first, whatever the mode.
    pub fn drain_follow_up(&self) -> Vec<Input> {
        lock(&self.follow_up).drain(..).collect()
    }

    /// Take every queued message: steering first, then follow-ups, each oldest first.
    pub(crate) fn take_all(&self) -> Vec<Input> {
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
    pub(crate) fn front(&self, queue: Queue) -> Option<Input> {
        lock(self.lane(queue)).front().cloned()
    }

    /// Remove the oldest message of `queue` after the session recorded it, unless a frontend
    /// drained the queue in the meantime and it now starts with another message.
    pub(crate) fn remove_front(&self, queue: Queue, message: &Input) {
        let mut messages = lock(self.lane(queue));
        if messages.front() == Some(message) {
            messages.pop_front();
        }
    }

    pub(crate) fn has(&self, queue: Queue) -> bool {
        !lock(self.lane(queue)).is_empty()
    }

    fn lane(&self, queue: Queue) -> &Mutex<VecDeque<Input>> {
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

    fn texts(messages: Vec<Input>) -> Vec<String> {
        messages
            .into_iter()
            .map(|message| match message {
                Input::User { text, .. } => text,
                other @ Input::Custom(_) => panic!("expected user messages, got {other:?}"),
            })
            .collect()
    }

    #[test]
    fn draining_takes_messages_oldest_first_and_empties_the_queue() {
        let queues = InputQueues::default();
        queues.steer("a");
        queues.steer("b");
        queues.follow_up("c");

        assert_eq!(texts(queues.drain_steering()), ["a", "b"]);
        assert!(queues.drain_steering().is_empty());
        assert_eq!(texts(queues.drain_follow_up()), ["c"]);
    }

    #[test]
    fn the_oldest_message_stays_queued_until_removed() {
        let queues = InputQueues::default();
        queues.steer("a");
        queues.steer("b");

        assert_eq!(queues.front(Queue::Steering), Some("a".into()));
        assert_eq!(queues.front(Queue::Steering), Some("a".into()));
        queues.remove_front(Queue::Steering, &"a".into());
        assert_eq!(queues.front(Queue::Steering), Some("b".into()));
        assert!(!queues.has(Queue::FollowUp));
    }

    #[test]
    fn withdrawing_removes_the_oldest_equal_message_from_the_steering_queue_first() {
        let queues = InputQueues::default();
        queues.steer("a");
        queues.steer("b");
        queues.steer("a");
        queues.follow_up("a");

        assert!(queues.withdraw(&"a".into()));
        assert_eq!(texts(queues.drain_steering()), ["b", "a"]);
        assert!(queues.withdraw(&"a".into()));
        assert!(!queues.has(Queue::FollowUp));
    }

    #[test]
    fn withdrawing_a_message_that_is_not_queued_changes_nothing() {
        let queues = InputQueues::default();
        queues.follow_up("a");

        assert!(!queues.withdraw(&"b".into()));
        let custom = CustomMessage {
            custom_type: "note".into(),
            content: "a".into(),
            display: false,
            details: None,
        };
        assert!(!queues.withdraw(&Input::Custom(custom)));
        assert_eq!(texts(queues.drain_follow_up()), ["a"]);
    }

    #[test]
    fn removing_after_a_drain_leaves_newer_input_queued() {
        let queues = InputQueues::default();
        queues.follow_up("a");
        // A frontend drains the queue and queues other input while the loop records "a".
        queues.drain_follow_up();
        queues.follow_up("b");

        queues.remove_front(Queue::FollowUp, &"a".into());

        assert_eq!(texts(queues.drain_follow_up()), ["b"]);
    }

    #[test]
    fn custom_messages_and_entries_queue_beside_user_text() {
        let queues = InputQueues::default();
        let custom = CustomMessage {
            custom_type: "note".into(),
            content: "remember".into(),
            display: false,
            details: None,
        };
        queues.steer("a");
        queues.steer_message(Input::Custom(custom.clone()));
        queues.append_entry("state", serde_json::json!({"n": 1}));

        assert_eq!(queues.take_all(), [Input::from("a"), Input::Custom(custom)]);
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
    fn the_queue_mode_defaults_to_all_and_can_be_changed() {
        let queues = InputQueues::default();
        assert_eq!(queues.mode(), QueueMode::All);
        queues.set_mode(QueueMode::OneAtATime);
        assert_eq!(queues.mode(), QueueMode::OneAtATime);
    }
}
