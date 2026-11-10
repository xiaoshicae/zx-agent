//! The subscribers of a conversation's events: the hub that fans each event out and the
//! handle that keeps a subscriber attached.

use std::panic::{AssertUnwindSafe, catch_unwind};
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
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(events) = self.events.upgrade() {
            events.detach(self.listener.as_ptr());
        }
    }
}

/// One attached sink. Each attachment is its own allocation, so its address identifies it
/// even when the same sink is attached twice.
struct Listener {
    sink: Arc<dyn EventSink>,
}

/// Fans each event out to the attached subscribers, detaching one that panics.
#[derive(Default)]
pub(super) struct EventHub {
    listeners: Mutex<Vec<Arc<Listener>>>,
}

impl EventHub {
    /// Attach `sink` for the hub's lifetime, so that the session's own sink never
    /// unsubscribes.
    pub(super) fn attach_permanently(&self, sink: Arc<dyn EventSink>) {
        drop(self.attach(sink));
    }

    /// Attach `sink`. Returns the listener's handle, for a [`Subscription`] that detaches it
    /// again.
    fn attach(&self, sink: Arc<dyn EventSink>) -> Weak<Listener> {
        let listener = Arc::new(Listener { sink });
        let handle = Arc::downgrade(&listener);
        lock(&self.listeners).push(listener);
        handle
    }

    /// Attach `sink` until the returned subscription is dropped.
    pub(super) fn subscribe(self: &Arc<Self>, sink: Arc<dyn EventSink>) -> Subscription {
        Subscription {
            events: Arc::downgrade(self),
            listener: self.attach(sink),
        }
    }

    /// Detach the listener at `address`. A weak reference or a snapshot keeps the
    /// allocation, so no other listener can share the address meanwhile.
    fn detach(&self, address: *const Listener) {
        lock(&self.listeners).retain(|listener| !std::ptr::eq(Arc::as_ptr(listener), address));
    }
}

impl EventSink for EventHub {
    fn emit(&self, event: &Event) {
        // Snapshot only the Arc handles. Reentrant subscribe, unsubscribe and abort must
        // never run under this lock. The snapshot also keeps detached sinks alive until
        // after unlocking, so even a custom destructor cannot run while the list is locked.
        let listeners = lock(&self.listeners).clone();
        for listener in &listeners {
            if catch_unwind(AssertUnwindSafe(|| listener.sink.emit(event))).is_err() {
                self.detach(Arc::as_ptr(listener));
            }
        }
    }
}
