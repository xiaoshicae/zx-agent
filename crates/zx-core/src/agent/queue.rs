//! Messages injected into a running loop.

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::ai::Message;
use crate::sync::lock;

/// Messages injected into a running loop (pi: `Agent.steeringQueue` / `followUpQueue`).
///
/// - Steering messages are taken after each turn and enter the conversation before the
///   next model request, while the task is still in progress.
/// - Follow-up messages are taken only when the agent would otherwise stop.
#[derive(Default)]
pub struct MessageQueues {
    steering: Mutex<VecDeque<String>>,
    follow_up: Mutex<VecDeque<String>>,
}

impl MessageQueues {
    /// Queue a user message for the next turn of the current task.
    pub fn steer(&self, text: impl Into<String>) {
        lock(&self.steering).push_back(text.into());
    }

    /// Queue a user message for after the current task finishes.
    pub fn follow_up(&self, text: impl Into<String>) {
        lock(&self.follow_up).push_back(text.into());
    }

    /// Take every queued steering message, oldest first.
    pub fn drain_steering(&self) -> Vec<Message> {
        drain(&self.steering)
    }

    /// Take every queued follow-up message, oldest first.
    pub fn drain_follow_up(&self) -> Vec<Message> {
        drain(&self.follow_up)
    }

    /// Take the text of every queued message: steering first, then follow-ups, each oldest
    /// first.
    pub(crate) fn take_all(&self) -> Vec<String> {
        let mut texts: Vec<String> = lock(&self.steering).drain(..).collect();
        texts.extend(lock(&self.follow_up).drain(..));
        texts
    }

    /// The oldest message of `queue`, left in place: the loop removes it only once the
    /// session has recorded it, so input that a cancelled or dropped run never recorded is
    /// still queued for the next run.
    pub(crate) fn front(&self, queue: Queue) -> Option<String> {
        lock(self.queue(queue)).front().cloned()
    }

    /// Remove the oldest message of `queue` after the session recorded it, unless a frontend
    /// drained the queue in the meantime and it now starts with other text.
    pub(crate) fn remove_front(&self, queue: Queue, text: &str) {
        let mut queue = lock(self.queue(queue));
        if queue.front().is_some_and(|front| front == text) {
            queue.pop_front();
        }
    }

    pub(crate) fn has(&self, queue: Queue) -> bool {
        !lock(self.queue(queue)).is_empty()
    }

    fn queue(&self, queue: Queue) -> &Mutex<VecDeque<String>> {
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

fn drain(queue: &Mutex<VecDeque<String>>) -> Vec<Message> {
    lock(queue).drain(..).map(Message::user).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(messages: Vec<Message>) -> Vec<String> {
        messages
            .into_iter()
            .map(|message| match message {
                Message::User { content, .. } => content,
                other => panic!("queues hold only user messages, got {other:?}"),
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

        assert_eq!(queues.front(Queue::Steering).as_deref(), Some("a"));
        assert_eq!(queues.front(Queue::Steering).as_deref(), Some("a"));
        queues.remove_front(Queue::Steering, "a");
        assert_eq!(queues.front(Queue::Steering).as_deref(), Some("b"));
        assert!(!queues.has(Queue::FollowUp));
    }

    #[test]
    fn removing_after_a_drain_leaves_newer_input_queued() {
        let queues = MessageQueues::default();
        queues.follow_up("a");
        // A frontend drains the queue and queues other input while the loop records "a".
        queues.drain_follow_up();
        queues.follow_up("b");

        queues.remove_front(Queue::FollowUp, "a");

        assert_eq!(texts(queues.drain_follow_up()), ["b"]);
    }
}
