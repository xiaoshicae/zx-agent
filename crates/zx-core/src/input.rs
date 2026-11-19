//! The messages given to a run, the one that starts it and those injected while it runs, the
//! queues that hold them, and the records an extension asks the loop to keep.

use std::any::Any;
use std::collections::VecDeque;
use std::sync::Mutex;

use serde_json::Value;

use crate::sync::lock;
use crate::transcript::{BlankCustomType, CustomMessage, check_custom_type};

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

/// Input waiting in a queue, or to start a run ([`Session::send`](crate::Session::send)), with
/// what the host attached to it, if anything: a hold, such as a share of the host's capacity,
/// that ends when the input leaves the queues: once it is recorded, withdrawn or taken without
/// it ([`PendingInput::into_input`]), or when the queues are dropped. Ending a hold drops it,
/// never under a queue's lock.
#[derive(Debug)]
pub struct PendingInput {
    input: Input,
    hold: Option<Box<dyn Any + Send>>,
}

impl PendingInput {
    /// `input` with `hold`, which lasts while the input waits.
    pub fn held(input: impl Into<Input>, hold: impl Any + Send) -> Self {
        Self {
            input: input.into(),
            hold: Some(Box::new(hold)),
        }
    }

    /// The input, while it waits.
    pub fn input(&self) -> &Input {
        &self.input
    }

    /// The input, ending its hold.
    pub fn into_input(self) -> Input {
        let Self { input, hold } = self;
        drop(hold);
        input
    }

    /// The input and its hold, which is the host's again: for a host that joins several
    /// inputs into one and keeps their holds with it.
    pub fn into_parts(self) -> (Input, Option<Box<dyn Any + Send>>) {
        (self.input, self.hold)
    }
}

impl From<Input> for PendingInput {
    fn from(input: Input) -> Self {
        Self { input, hold: None }
    }
}

impl From<String> for PendingInput {
    fn from(text: String) -> Self {
        Self::from(Input::from(text))
    }
}

impl From<&str> for PendingInput {
    fn from(text: &str) -> Self {
        Self::from(Input::from(text))
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
    steering: Mutex<VecDeque<PendingInput>>,
    follow_up: Mutex<VecDeque<PendingInput>>,
    mode: Mutex<QueueMode>,
    entries: Mutex<Vec<PendingEntry>>,
}

impl InputQueues {
    /// Queue a message for the next step of the current task: the person's text, any
    /// [`Input`], or input with a hold of the host's ([`PendingInput::held`]).
    pub fn steer(&self, message: impl Into<PendingInput>) {
        lock(&self.steering).push_back(message.into());
    }

    /// Queue a message for after the current task finishes, as [`steer`](Self::steer) takes
    /// it.
    pub fn follow_up(&self, message: impl Into<PendingInput>) {
        lock(&self.follow_up).push_back(message.into());
    }

    /// Ask the loop to record state of an extension (pi: `appendEntry`) at the start of its
    /// next step, or when the run ends. Without a run the entry waits for the next one; a
    /// host that is idle records directly through its session instead. A blank
    /// `custom_type` is refused here, and nothing is queued: recorded, it would fail the run.
    pub fn append_custom_entry(&self, custom_type: impl Into<String>, data: Value) -> Result<(), BlankCustomType> {
        let custom_type = custom_type.into();
        check_custom_type(&custom_type)?;
        lock(&self.entries).push(PendingEntry { custom_type, data });
        Ok(())
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
            let withdrawn = {
                let mut messages = lock(self.lane(queue));
                let index = messages.iter().position(|queued| &queued.input == message);
                index.and_then(|index| messages.remove(index))
            };
            withdrawn.is_some()
        })
    }

    /// Take every queued message with its hold: steering first, then follow-ups, each oldest
    /// first. A host that moves the messages into queues of its own keeps the holds with them.
    pub fn take_all(&self) -> Vec<PendingInput> {
        let mut messages = Self::take(&self.steering);
        messages.extend(Self::take(&self.follow_up));
        messages
    }

    /// Everything in `lane`, taken under its lock and handed out after it.
    fn take(lane: &Mutex<VecDeque<PendingInput>>) -> Vec<PendingInput> {
        lock(lane).drain(..).collect()
    }

    /// Take the entries waiting to be recorded, oldest first.
    pub(crate) fn take_entries(&self) -> Vec<PendingEntry> {
        std::mem::take(&mut *lock(&self.entries))
    }

    /// The oldest message of `queue`, left in place: the loop removes it only once the
    /// session has recorded it, so input that a cancelled or dropped run never recorded is
    /// still queued for the next run.
    pub(crate) fn front(&self, queue: Queue) -> Option<Input> {
        lock(self.lane(queue)).front().map(|pending| pending.input.clone())
    }

    /// Remove the oldest message of `queue` after the session recorded it, unless a frontend
    /// drained the queue in the meantime and it now starts with another message. Its hold
    /// ends here, after the queue's lock is released.
    pub(crate) fn remove_front(&self, queue: Queue, message: &Input) {
        let removed = {
            let mut messages = lock(self.lane(queue));
            if messages.front().is_some_and(|pending| &pending.input == message) {
                messages.pop_front()
            } else {
                None
            }
        };
        drop(removed);
    }

    pub(crate) fn has(&self, queue: Queue) -> bool {
        !lock(self.lane(queue)).is_empty()
    }

    fn lane(&self, queue: Queue) -> &Mutex<VecDeque<PendingInput>> {
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// The inputs of `messages`, whose holds end here, outside any queue's lock.
    fn inputs(messages: Vec<PendingInput>) -> Vec<Input> {
        messages.into_iter().map(PendingInput::into_input).collect()
    }

    /// Take every message `queue` holds, oldest first, whatever the mode, ending their holds.
    fn drain(queues: &InputQueues, queue: Queue) -> Vec<Input> {
        inputs(InputQueues::take(queues.lane(queue)))
    }

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

        assert_eq!(texts(drain(&queues, Queue::Steering)), ["a", "b"]);
        assert!(drain(&queues, Queue::Steering).is_empty());
        assert_eq!(texts(drain(&queues, Queue::FollowUp)), ["c"]);
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
        assert_eq!(texts(drain(&queues, Queue::Steering)), ["b", "a"]);
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
        assert_eq!(texts(drain(&queues, Queue::FollowUp)), ["a"]);
    }

    #[test]
    fn removing_after_a_drain_leaves_newer_input_queued() {
        let queues = InputQueues::default();
        queues.follow_up("a");
        // A frontend drains the queue and queues other input while the loop records "a".
        drain(&queues, Queue::FollowUp);
        queues.follow_up("b");

        queues.remove_front(Queue::FollowUp, &"a".into());

        assert_eq!(texts(drain(&queues, Queue::FollowUp)), ["b"]);
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
        queues.steer(Input::Custom(custom.clone()));
        queues
            .append_custom_entry("state", serde_json::json!({"n": 1}))
            .unwrap();
        assert_eq!(
            queues.append_custom_entry(" ", serde_json::json!(null)),
            Err(BlankCustomType)
        );

        assert_eq!(inputs(queues.take_all()), [Input::from("a"), Input::Custom(custom)]);
        assert_eq!(
            queues.take_entries(),
            [PendingEntry {
                custom_type: "state".into(),
                data: serde_json::json!({"n": 1})
            }]
        );
        assert!(queues.take_entries().is_empty());
    }

    /// A hold that counts how many of its kind are alive.
    struct Hold(Arc<AtomicUsize>);

    impl Hold {
        fn new(alive: &Arc<AtomicUsize>) -> Self {
            alive.fetch_add(1, Ordering::SeqCst);
            Self(Arc::clone(alive))
        }
    }

    impl Drop for Hold {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn a_hold_moves_with_its_input_and_ends_when_the_input_leaves_the_queues() {
        let alive = Arc::new(AtomicUsize::new(0));
        let queues = InputQueues::default();
        queues.steer(PendingInput::held("a", Hold::new(&alive)));
        queues.follow_up(PendingInput::held("b", Hold::new(&alive)));

        let taken = queues.take_all();
        assert_eq!(alive.load(Ordering::SeqCst), 2, "taking keeps the holds");
        for pending in taken {
            queues.steer(pending);
        }
        queues.remove_front(Queue::Steering, &"a".into());
        assert_eq!(alive.load(Ordering::SeqCst), 1, "recorded input lets go of its hold");
        assert!(queues.withdraw(&"b".into()));
        assert_eq!(alive.load(Ordering::SeqCst), 0, "so does withdrawn input");

        queues.steer(PendingInput::held("c", Hold::new(&alive)));
        assert_eq!(texts(drain(&queues, Queue::Steering)), ["c"]);
        assert_eq!(alive.load(Ordering::SeqCst), 0, "and input taken without it");
        queues.follow_up(PendingInput::held("d", Hold::new(&alive)));
        drop(queues);
        assert_eq!(alive.load(Ordering::SeqCst), 0, "and input dropped with the queues");
    }

    #[test]
    fn a_hold_taken_apart_from_its_input_lasts_as_long_as_the_host_keeps_it() {
        let alive = Arc::new(AtomicUsize::new(0));
        let (input, hold) = PendingInput::held("a", Hold::new(&alive)).into_parts();
        assert_eq!(input, Input::from("a"));
        assert_eq!(alive.load(Ordering::SeqCst), 1);
        assert!(hold.is_some_and(|hold| hold.is::<Hold>()));
        assert_eq!(alive.load(Ordering::SeqCst), 0);
        assert!(PendingInput::from("b").into_parts().1.is_none());
    }

    #[test]
    fn the_queue_mode_defaults_to_all_and_can_be_changed() {
        let queues = InputQueues::default();
        assert_eq!(queues.mode(), QueueMode::All);
        queues.set_mode(QueueMode::OneAtATime);
        assert_eq!(queues.mode(), QueueMode::OneAtATime);
    }
}
