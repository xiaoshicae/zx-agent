//! The subscribers of a conversation's events: the hub that fans each event out, the handle
//! that keeps a subscriber attached, and the bounded stream for exporters.

use std::num::NonZeroUsize;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

use crate::agent::{Event, EventSink};
use crate::sync::lock;

/// Keeps an additional event subscriber attached. Dropping it unsubscribes; an already
/// dispatched snapshot may still finish. The handle keeps neither the conversation nor the
/// sink alive: the session owns its subscribers and releases them when it is dropped.
#[must_use = "retain the subscription for as long as events are needed"]
pub struct Subscription {
    events: Weak<EventHub>,
    /// Identifies the subscriber; the session holds the only strong reference.
    listener: Weak<Listener>,
    failed: Arc<AtomicBool>,
}

impl Subscription {
    /// Whether a callback panic caused this subscriber to be detached.
    pub fn has_panicked(&self) -> bool {
        self.failed.load(Ordering::SeqCst)
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(events) = self.events.upgrade() {
            // The weak reference keeps the allocation, so no other listener can share its address.
            lock(&events.listeners).retain(|listener| !std::ptr::eq(Arc::as_ptr(listener), self.listener.as_ptr()));
        }
    }
}

struct Listener {
    sink: Arc<dyn EventSink>,
    failed: Arc<AtomicBool>,
}

/// Fans each event out to the attached subscribers, isolating one that panics.
#[derive(Default)]
pub(super) struct EventHub {
    listeners: Mutex<Vec<Arc<Listener>>>,
    failures: AtomicUsize,
}

impl EventHub {
    /// Attach `sink` for the hub's lifetime, so that the session's own sink never
    /// unsubscribes.
    pub(super) fn attach_permanently(&self, sink: Arc<dyn EventSink>) {
        drop(self.attach(sink));
    }

    /// Attach `sink`. Returns the listener's handle and its failure flag, for a
    /// [`Subscription`] that detaches it again.
    fn attach(&self, sink: Arc<dyn EventSink>) -> (Weak<Listener>, Arc<AtomicBool>) {
        let failed = Arc::new(AtomicBool::new(false));
        let listener = Arc::new(Listener {
            sink,
            failed: failed.clone(),
        });
        let handle = Arc::downgrade(&listener);
        lock(&self.listeners).push(listener);
        (handle, failed)
    }

    /// Attach `sink` until the returned subscription is dropped.
    pub(super) fn subscribe(self: &Arc<Self>, sink: Arc<dyn EventSink>) -> Subscription {
        let (listener, failed) = self.attach(sink);
        Subscription {
            events: Arc::downgrade(self),
            listener,
            failed,
        }
    }

    /// Attach a channel that holds at most `capacity` events; see
    /// [`AgentSession::subscribe_bounded`](crate::AgentSession::subscribe_bounded).
    pub(super) fn subscribe_bounded(self: &Arc<Self>, capacity: NonZeroUsize) -> EventStream {
        let (sender, receiver) = tokio::sync::mpsc::channel(capacity.get());
        let dropped = Arc::new(AtomicU64::new(0));
        let sink = Arc::new(BoundedSink {
            sender,
            dropped: dropped.clone(),
        });
        EventStream {
            receiver,
            dropped,
            _subscription: self.subscribe(sink),
        }
    }

    /// Number of subscribers detached because their callback panicked.
    pub(super) fn failures(&self) -> usize {
        self.failures.load(Ordering::SeqCst)
    }
}

/// Events of an [`AgentSession`](crate::AgentSession) through a bounded channel; see
/// [`AgentSession::subscribe_bounded`](crate::AgentSession::subscribe_bounded).
pub struct EventStream {
    receiver: tokio::sync::mpsc::Receiver<Event>,
    // Shared with the sink that counts: an atomic counter, read here and written there.
    dropped: Arc<AtomicU64>,
    _subscription: Subscription,
}

impl EventStream {
    /// The next event; waits until one arrives. `None` once the session is gone and the
    /// events it sent before are read.
    pub async fn recv(&mut self) -> Option<Event> {
        self.receiver.recv().await
    }

    /// The next event if one is waiting, without waiting.
    pub fn try_recv(&mut self) -> Option<Event> {
        self.receiver.try_recv().ok()
    }

    /// Events dropped so far because the channel was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::SeqCst)
    }
}

struct BoundedSink {
    sender: tokio::sync::mpsc::Sender<Event>,
    dropped: Arc<AtomicU64>,
}

impl EventSink for BoundedSink {
    fn emit(&self, event: &Event) {
        match self.sender.try_send(event.clone()) {
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::SeqCst);
            }
            // A closed stream is gone; its subscription is being dropped as well.
            Ok(()) | Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
        }
    }
}

impl EventSink for EventHub {
    fn emit(&self, event: &Event) {
        // Snapshot only the Arc handles. Reentrant subscribe, unsubscribe and abort must
        // never run under this lock.
        let listeners = lock(&self.listeners).clone();
        let mut failed = false;
        for listener in &listeners {
            if listener.failed.load(Ordering::SeqCst) {
                continue;
            }
            if catch_unwind(AssertUnwindSafe(|| listener.sink.emit(event))).is_err() {
                if !listener.failed.swap(true, Ordering::SeqCst) {
                    self.failures.fetch_add(1, Ordering::SeqCst);
                }
                failed = true;
            }
        }
        if failed {
            // Snapshot references keep removed sinks alive until after unlocking, so even
            // a custom destructor cannot run while the subscriber list is locked.
            lock(&self.listeners).retain(|listener| !listener.failed.load(Ordering::SeqCst));
        }
    }
}
