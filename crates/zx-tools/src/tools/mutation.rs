//! One change at a time per file, so that two calls of a batch cannot overwrite each other's
//! edit (pi: coding-agent/src/core/tools/file-mutation-queue.ts).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::OwnedMutexGuard;

use crate::ai::sync::lock;

/// Serializes the changes to each file among the tools that share it. Changes to different
/// files run at the same time.
#[derive(Default)]
pub struct FileMutationQueue {
    /// One lock per file being changed. Shared (`Arc`) by the calls that wait for it; the map
    /// holds it weakly, so it goes when its last holder does.
    locks: Mutex<HashMap<PathBuf, Weak<tokio::sync::Mutex<()>>>>,
}

impl FileMutationQueue {
    /// Wait for the turn to change `path`. Paths that name the same file through symbolic
    /// links or `..` share a turn; for a file that does not exist yet, its directory is
    /// resolved instead. The turn lasts as long as the returned guard.
    pub async fn turn(&self, path: &Path) -> OwnedMutexGuard<()> {
        let key = canonical(path).await;
        let file_lock = {
            let mut locks = lock(&self.locks);
            locks.retain(|_, held| held.strong_count() > 0);
            match locks.get(&key).and_then(Weak::upgrade) {
                Some(existing) => existing,
                None => {
                    let created = Arc::new(tokio::sync::Mutex::new(()));
                    locks.insert(key, Arc::downgrade(&created));
                    created
                }
            }
        };
        file_lock.lock_owned().await
    }
}

/// The real path of `path`, or of its directory joined with its name when the file does not
/// exist yet; `path` itself when neither resolves.
async fn canonical(path: &Path) -> PathBuf {
    if let Ok(real) = tokio::fs::canonicalize(path).await {
        return real;
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
        && let Ok(real) = tokio::fs::canonicalize(parent).await
    {
        return real.join(name);
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_same_file_waits_and_another_file_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let queue = FileMutationQueue::default();
        let first = queue.turn(&dir.path().join("a.txt")).await;
        // Another spelling of the same, not yet existing, file.
        let same = dir.path().join(".").join("a.txt");
        let waiting = tokio::time::timeout(std::time::Duration::from_millis(50), queue.turn(&same)).await;
        assert!(waiting.is_err(), "the second change must wait");
        let other =
            tokio::time::timeout(std::time::Duration::from_secs(5), queue.turn(&dir.path().join("b.txt"))).await;
        assert!(other.is_ok(), "another file must not wait");
        drop(first);
        let after = tokio::time::timeout(std::time::Duration::from_secs(5), queue.turn(&same)).await;
        assert!(after.is_ok(), "the turn passes when the first change ends");
    }

    #[tokio::test]
    async fn finished_turns_leave_no_lock_behind() {
        let dir = tempfile::tempdir().unwrap();
        let queue = FileMutationQueue::default();
        for name in ["a", "b", "c"] {
            drop(queue.turn(&dir.path().join(name)).await);
        }
        drop(queue.turn(&dir.path().join("d")).await);
        assert!(lock(&queue.locks).len() <= 1);
    }
}
