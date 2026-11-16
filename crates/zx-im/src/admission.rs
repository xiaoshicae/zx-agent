//! A conversation's share of pending input: how many messages, and how many bytes of text,
//! may wait for its session at once. A message reserves its share when the router takes it
//! and keeps it through the worker's queues into the session's, as the hold of its
//! [`PendingInput`], until the session records it or it is dropped.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use zx_ai::sync::lock;
use zx_core::input::{Input, PendingInput};

/// The limit on one conversation's pending messages and their UTF-8 text bytes. The router and
/// the worker reserve from it concurrently.
#[derive(Debug)]
pub(crate) struct InputBudget {
    max_messages: usize,
    max_bytes: usize,
    /// Messages and text bytes reserved now.
    used: Mutex<(usize, usize)>,
}

impl InputBudget {
    pub(crate) fn new(max_messages: NonZeroUsize, max_bytes: NonZeroUsize) -> Self {
        Self {
            max_messages: max_messages.get(),
            max_bytes: max_bytes.get(),
            used: Mutex::default(),
        }
    }

    /// Reserve one message of `bytes`; `None` when either limit would be passed. Dropping the
    /// permit returns both.
    pub(crate) fn try_reserve(self: &Arc<Self>, bytes: usize) -> Option<InputPermit> {
        let mut used = lock(&self.used);
        if used.0 >= self.max_messages || bytes > self.max_bytes - used.1 {
            return None;
        }
        used.0 += 1;
        used.1 += bytes;
        Some(InputPermit {
            budget: Arc::clone(self),
            bytes,
        })
    }

    /// `input` with a share of this budget as its hold; `None` when the budget is full.
    pub(crate) fn try_admit(self: &Arc<Self>, input: Input) -> Option<PendingInput> {
        let permit = self.try_reserve(text_bytes(&input))?;
        Some(PendingInput::held(input, permit))
    }

    /// Messages and text bytes reserved at this instant.
    #[cfg(test)]
    fn reserved(&self) -> (usize, usize) {
        *lock(&self.used)
    }
}

/// One message's share of an [`InputBudget`], returned when it is dropped. It moves with the
/// message and is never copied, so moving a message between queues admits nothing new.
#[derive(Debug)]
pub(crate) struct InputPermit {
    budget: Arc<InputBudget>,
    bytes: usize,
}

impl InputPermit {
    /// `input`, the message as the session will record it, holding this share resized to its
    /// text, which may have grown by the sender's name; `None`, the share returned, when the
    /// budget has no room for the larger text.
    pub(crate) fn attach(mut self, input: Input) -> Option<PendingInput> {
        let bytes = text_bytes(&input);
        {
            let mut used = lock(&self.budget.used);
            let without = used.1 - self.bytes;
            if bytes > self.budget.max_bytes - without {
                return None;
            }
            used.1 = without + bytes;
            self.bytes = bytes;
        }
        Some(PendingInput::held(input, self))
    }
}

impl Drop for InputPermit {
    fn drop(&mut self) {
        let mut used = lock(&self.budget.used);
        used.0 -= 1;
        used.1 -= self.bytes;
    }
}

/// The UTF-8 bytes of the text of `input`, which the budget counts.
fn text_bytes(input: &Input) -> usize {
    match input {
        Input::User { text, .. } => text.len(),
        Input::Custom(message) => message.content.len(),
    }
}

#[cfg(test)]
mod tests {
    use zx_core::input::InputQueues;

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
        queues.steer(inbox.attach("中文".into()).unwrap());
        assert_eq!(budget.reserved(), (1, 6));
        for pending in queues.take_all() {
            assert!(budget.try_reserve(1).is_none(), "taken back, still reserved");
            queues.follow_up(pending);
        }
        assert_eq!(budget.reserved(), (1, 6));
        assert_eq!(queues.drain_follow_up(), [Input::from("中文")]);
        assert_eq!(budget.reserved(), (0, 0));
    }

    #[test]
    fn a_name_that_does_not_fit_returns_the_reservation() {
        let budget = budget(2, 6);
        let first = budget.try_reserve(3).unwrap();
        let second = budget.try_reserve(3).unwrap();
        assert!(first.attach(Input::from("name:abc")).is_none());
        assert_eq!(budget.reserved(), (1, 3));
        drop(second);
        assert_eq!(budget.reserved(), (0, 0));
    }

    #[test]
    fn a_shrunk_reservation_and_withdrawn_input_return_their_share() {
        let budget = budget(2, 6);
        let first = budget.try_reserve(6).unwrap().attach("a".into()).unwrap();
        let queues = InputQueues::default();
        queues.steer(first);
        queues.follow_up(budget.try_admit("12345".into()).unwrap());
        assert!(budget.try_admit("".into()).is_none());
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
