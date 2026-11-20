//! A conversation's share of pending input: how many messages, and how many bytes of text,
//! may wait for its session at once. A message reserves its share when the router takes it
//! and keeps it through the worker's queues into the session's, held with its receipt from
//! the backlog, until the session records it or it is dropped. What the operator is told of a
//! message that is not accepted names the limit it met.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use zx_util::sync::lock;

/// The limit on one conversation's pending messages and their UTF-8 text bytes. The router and
/// the worker reserve from it concurrently.
#[derive(Debug)]
pub(super) struct InputBudget {
    max_messages: usize,
    max_bytes: usize,
    used: Mutex<Used>,
}

/// What a budget has reserved now.
#[derive(Debug, Default)]
struct Used {
    messages: usize,
    /// UTF-8 bytes of their text.
    bytes: usize,
}

impl InputBudget {
    pub(super) fn new(max_messages: NonZeroUsize, max_bytes: NonZeroUsize) -> Self {
        Self {
            max_messages: max_messages.get(),
            max_bytes: max_bytes.get(),
            used: Mutex::default(),
        }
    }

    /// Reserve one message of `bytes`; `None` when either limit would be passed. Dropping the
    /// permit returns both.
    pub(super) fn try_reserve(self: &Arc<Self>, bytes: usize) -> Option<InputPermit> {
        let mut used = lock(&self.used);
        if used.messages >= self.max_messages || bytes > self.max_bytes - used.bytes {
            return None;
        }
        used.messages += 1;
        used.bytes += bytes;
        Some(InputPermit {
            budget: Arc::clone(self),
            bytes,
        })
    }

    /// Messages and text bytes reserved at this instant.
    #[cfg(test)]
    fn reserved(&self) -> (usize, usize) {
        let used = lock(&self.used);
        (used.messages, used.bytes)
    }
}

/// Which limit a message that is not accepted met.
#[derive(Clone, Copy)]
pub(super) enum Limit {
    /// The message alone has more text than `max_pending_bytes`.
    MessageBytes,
    /// The conversation has `max_pending_messages` messages pending, or no room for the
    /// message's text within `max_pending_bytes`.
    Pending,
    /// The conversation's inbox is full: its worker takes input in slower than it comes.
    Inbox,
    /// `max_conversations` conversations are served.
    Conversations,
}

impl Limit {
    /// What the operator is told of a message not accepted for this limit, which names the
    /// setting of [`ServerConfig`](super::ServerConfig) that sets it.
    pub(super) fn refusal(self) -> &'static str {
        match self {
            Self::MessageBytes => "message not accepted: its text is longer than max_pending_bytes",
            Self::Pending => {
                "message not accepted: the conversation has max_pending_messages messages, or max_pending_bytes bytes, pending"
            }
            Self::Inbox => "message not accepted: the conversation's inbox is full",
            Self::Conversations => "message not accepted: max_conversations conversations are served",
        }
    }
}

/// One message's share of an [`InputBudget`], returned when it is dropped. It moves with the
/// message and is never copied, so moving a message between queues admits nothing new.
#[derive(Debug)]
pub(super) struct InputPermit {
    budget: Arc<InputBudget>,
    bytes: usize,
}

impl InputPermit {
    /// This share resized to `bytes`, the text of the message as the session will record it,
    /// which may have grown by the sender's name; `None`, the share returned, when the budget
    /// has no room for the larger text.
    pub(super) fn resize(mut self, bytes: usize) -> Option<Self> {
        {
            let mut used = lock(&self.budget.used);
            let without = used.bytes - self.bytes;
            if bytes > self.budget.max_bytes - without {
                return None;
            }
            used.bytes = without + bytes;
            self.bytes = bytes;
        }
        Some(self)
    }
}

impl Drop for InputPermit {
    fn drop(&mut self) {
        let mut used = lock(&self.budget.used);
        used.messages -= 1;
        used.bytes -= self.bytes;
    }
}

#[cfg(test)]
mod tests {
    use zx_agent::input::{Input, InputQueues, PendingInput};

    use super::*;

    fn budget(count: usize, bytes: usize) -> Arc<InputBudget> {
        Arc::new(InputBudget::new(count.try_into().unwrap(), bytes.try_into().unwrap()))
    }

    #[test]
    fn moving_between_queues_keeps_one_reservation() {
        let budget = budget(1, 6);
        let inbox = budget.try_reserve(6).unwrap();
        assert!(budget.try_reserve(0).is_none());
        let queues = InputQueues::default();
        queues.steer(PendingInput::held("中文", inbox.resize("中文".len()).unwrap()));
        assert_eq!(budget.reserved(), (1, 6));
        for pending in queues.take_all() {
            assert!(budget.try_reserve(1).is_none(), "taken back, still reserved");
            queues.follow_up(pending);
        }
        assert_eq!(budget.reserved(), (1, 6));
        let taken: Vec<Input> = queues.take_all().into_iter().map(PendingInput::into_input).collect();
        assert_eq!(taken, [Input::from("中文")]);
        assert_eq!(budget.reserved(), (0, 0));
    }

    #[test]
    fn a_name_that_does_not_fit_returns_the_reservation() {
        let budget = budget(2, 6);
        let first = budget.try_reserve(3).unwrap();
        let second = budget.try_reserve(3).unwrap();
        assert!(first.resize("name:abc".len()).is_none());
        assert_eq!(budget.reserved(), (1, 3));
        drop(second);
        assert_eq!(budget.reserved(), (0, 0));
    }

    #[test]
    fn a_shrunk_reservation_returns_the_difference() {
        let budget = budget(2, 6);
        let shrunk = budget.try_reserve(6).unwrap().resize(1).unwrap();
        assert_eq!(budget.reserved(), (1, 1));
        drop(shrunk);
        assert_eq!(budget.reserved(), (0, 0));
    }

    #[test]
    fn withdrawn_input_returns_its_share() {
        let budget = budget(2, 6);
        let queues = InputQueues::default();
        queues.steer(PendingInput::held("a", budget.try_reserve(1).unwrap()));
        queues.follow_up(PendingInput::held("12345", budget.try_reserve(5).unwrap()));
        assert!(budget.try_reserve(0).is_none());
        assert!(queues.withdraw(&"a".into()));
        assert_eq!(budget.reserved(), (1, 5));
        drop(queues);
        assert_eq!(budget.reserved(), (0, 0));
    }

    #[test]
    fn concurrent_producers_cannot_pass_either_limit() {
        let budget = budget(8, 18);
        let permits = std::thread::scope(|scope| {
            let producers: Vec<_> = (0..16)
                .map(|_| {
                    let budget = Arc::clone(&budget);
                    scope.spawn(move || budget.try_reserve(3))
                })
                .collect();
            producers
                .into_iter()
                .filter_map(|producer| producer.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(permits.len(), 6);
        assert_eq!(budget.reserved(), (6, 18));
        drop(permits);
        assert_eq!(budget.reserved(), (0, 0));
    }
}
