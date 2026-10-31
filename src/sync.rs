//! Synchronization helpers shared across the crate.

use std::sync::{Mutex, MutexGuard};

use tokio::task::JoinHandle;

/// A background task owned by an operation. Dropping a plain JoinHandle detaches it;
/// these tasks must instead stop when their run or tool future is dropped.
pub(crate) struct AbortOnDrop<T>(pub(crate) JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Lock `mutex`, recovering the data if a previous holder panicked.
///
/// Poisoning only records that some holder panicked. Every critical section in this crate
/// is a single operation (push, take, assign) or an append that does not panic midway, so
/// the data is never left half-updated and stays safe to use. Do not use this helper for a
/// critical section that could leave partial state behind.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn lock_recovers_a_poisoned_mutex() {
        let mutex = Arc::new(Mutex::new(1));
        let shared = mutex.clone();
        // A direct `.lock()` here, because poisoning is what the test sets up.
        let panicked = std::thread::spawn(move || {
            let _guard = shared.lock().unwrap();
            panic!("poison the mutex");
        })
        .join();

        assert!(panicked.is_err() && mutex.is_poisoned());
        assert_eq!(*lock(&mutex), 1);
    }
}
