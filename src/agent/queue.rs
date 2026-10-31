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
}
