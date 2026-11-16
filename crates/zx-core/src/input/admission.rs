//! A pending message's share of a host's input budget, kept as the message changes queues.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use super::Input;
use crate::sync::lock;

/// A shared, optional limit on pending message count and UTF-8 text bytes. Reserve before
/// putting input into a host's inbox, then move the permit with it into [`PendingInput`].
/// Running input and custom entry records are outside this budget.
#[derive(Debug)]
pub struct InputBudget {
    max_messages: usize,
    max_bytes: usize,
    used: Mutex<(usize, usize)>,
}

impl InputBudget {
    /// A budget with positive limits; several producers may reserve from it concurrently.
    pub fn new(max_messages: NonZeroUsize, max_bytes: NonZeroUsize) -> Self {
        Self {
            max_messages: max_messages.get(),
            max_bytes: max_bytes.get(),
            used: Mutex::default(),
        }
    }

    /// Reserve one message of `bytes`; `None` when either limit would be exceeded. Dropping
    /// the permit returns both resources, including when an inbox rejects its message.
    pub fn try_reserve(self: &Arc<Self>, bytes: usize) -> Option<InputPermit> {
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

    /// Admit input directly, returning it unchanged when the budget is full.
    pub fn try_admit(self: &Arc<Self>, input: Input) -> Result<PendingInput, InputRejected> {
        match self.try_reserve(input.text_bytes()) {
            Some(permit) => Ok(PendingInput {
                input,
                permit: Some(permit),
            }),
            None => Err(InputRejected { input: Box::new(input) }),
        }
    }

    /// The count and text bytes reserved at this instant, from one atomic snapshot.
    pub fn pending_size(&self) -> (usize, usize) {
        *lock(&self.used)
    }
}

/// Admission failed without consuming the input. The payload is boxed only on rejection,
/// so errors remain small even when custom messages carry structured details.
#[derive(Debug, thiserror::Error)]
#[error("pending input budget exhausted")]
pub struct InputRejected {
    input: Box<Input>,
}

impl InputRejected {
    /// Recover the original input for a retry or an explicit rejection response.
    pub fn into_input(self) -> Input {
        *self.input
    }
}

/// A reservation owned by one pending message. It is intentionally neither cloneable nor
/// serializable; moving it to another queue does not admit another message.
#[derive(Debug)]
pub struct InputPermit {
    budget: Arc<InputBudget>,
    bytes: usize,
}

impl InputPermit {
    /// Attach the final input, accounting for any added attribution text. On insufficient
    /// capacity the input is returned and the original reservation is released.
    pub fn attach(mut self, input: Input) -> Result<PendingInput, InputRejected> {
        let bytes = input.text_bytes();
        {
            let mut used = lock(&self.budget.used);
            let without = used.1 - self.bytes;
            if bytes > self.budget.max_bytes - without {
                return Err(InputRejected { input: Box::new(input) });
            }
            used.1 = without + bytes;
            self.bytes = bytes;
        }
        Ok(PendingInput {
            input,
            permit: Some(self),
        })
    }
}

impl Drop for InputPermit {
    fn drop(&mut self) {
        let mut used = lock(&self.budget.used);
        used.0 -= 1;
        used.1 -= self.bytes;
    }
}

/// Input moving between a host's queues and the session queues, with its optional budget
/// reservation. Reading it borrows the input; consuming or dropping it releases the permit.
#[derive(Debug)]
pub struct PendingInput {
    pub(super) input: Input,
    // Ownership, rather than a callback on the consumer, releases the reservation.
    permit: Option<InputPermit>,
}

impl PendingInput {
    /// Input without a budget reservation, as the default SDK queues accept.
    pub fn unbudgeted(input: Input) -> Self {
        Self { input, permit: None }
    }

    /// The input while its reservation remains held.
    pub fn input(&self) -> &Input {
        &self.input
    }

    /// Consume the input, returning its reservation to the budget.
    pub fn into_input(self) -> Input {
        let Self { input, permit } = self;
        drop(permit);
        input
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{InputQueues, Queue};

    fn budget(count: usize, bytes: usize) -> Arc<InputBudget> {
        Arc::new(InputBudget::new(count.try_into().unwrap(), bytes.try_into().unwrap()))
    }

    #[test]
    fn moving_between_host_and_session_queues_keeps_one_reservation() {
        let budget = budget(1, 6);
        let inbox = budget.try_reserve(6).unwrap();
        assert!(budget.try_reserve(0).is_none());
        let queues = InputQueues::default();
        queues.steer_pending(inbox.attach("中文".into()).unwrap());
        assert_eq!(budget.pending_size(), (1, 6));
        let mut waiting = queues.drain_pending();
        assert!(budget.try_reserve(1).is_none());
        queues.follow_up_pending(waiting.pop().unwrap());
        let input = queues.front(Queue::FollowUp).unwrap();
        assert_eq!(budget.pending_size(), (1, 6));
        queues.remove_front(Queue::FollowUp, &input);
        assert_eq!(budget.pending_size(), (0, 0));
    }

    #[test]
    fn rejected_attribution_releases_the_original_reservation() {
        let budget = budget(2, 6);
        let first = budget.try_reserve(3).unwrap();
        let second = budget.try_reserve(3).unwrap();
        let input = Input::from("name:abc");
        assert_eq!(first.attach(input.clone()).unwrap_err().into_input(), input);
        assert_eq!(budget.pending_size(), (1, 3));
        drop(second);
        assert_eq!(budget.pending_size(), (0, 0));
    }

    #[test]
    fn shrinking_a_reservation_and_withdrawing_input_returns_capacity() {
        let budget = budget(2, 6);
        let first = budget.try_reserve(6).unwrap().attach("a".into()).unwrap();
        let queues = InputQueues::default();
        queues.steer_pending(first);
        queues.follow_up_pending(budget.try_admit("12345".into()).unwrap());
        assert!(budget.try_admit("".into()).is_err());
        assert!(queues.withdraw(&"a".into()));
        assert_eq!(budget.pending_size(), (1, 5));
        drop(queues);
        assert_eq!(budget.pending_size(), (0, 0));
    }

    #[test]
    fn concurrent_producers_cannot_exceed_either_limit() {
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
        assert_eq!(budget.pending_size(), (6, 18));
        drop(permits);
        assert_eq!(budget.pending_size(), (0, 0));
    }
}
