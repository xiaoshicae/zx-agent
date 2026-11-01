//! Bounded capture of a child process's output streams and child cleanup, shared by the
//! executors.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Child;

use super::Captured;
use crate::sync::{AbortOnDrop, lock};

/// Bytes kept per stream while reading. Older output is dropped; only the tail is returned.
const CAPTURE_LIMIT: usize = 1024 * 1024;
/// How long to wait for output pipes after the process ends. Background children that
/// inherited the pipes could otherwise keep them open forever.
const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Output read so far, shared with the task that reads it.
pub(super) type Shared = Arc<Mutex<Captured>>;

/// Start reading `reader` into a bounded buffer. The task stops at end of stream or on a
/// read error; the buffer keeps what was read even if the task is aborted.
pub(super) fn spawn_reader<R>(reader: Option<R>) -> (Shared, Option<AbortOnDrop<()>>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let capture = Arc::new(Mutex::new(Captured::default()));
    let Some(mut reader) = reader else {
        return (capture, None);
    };
    // Shared buffer: output read so far survives even if the reader task is aborted.
    let shared = capture.clone();
    let handle = tokio::spawn(async move {
        let mut chunk = [0u8; 8192];
        // A read error ends the capture early; the exit status still reports how the command ended.
        while let Ok(n) = reader.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            let mut capture = lock(&shared);
            capture.bytes.extend_from_slice(&chunk[..n]);
            if capture.bytes.len() > CAPTURE_LIMIT {
                let excess = capture.bytes.len() - CAPTURE_LIMIT;
                capture.bytes.drain(..excess);
                capture.dropped += excess;
            }
        }
    });
    (capture, Some(AbortOnDrop(handle)))
}

/// Wait up to the drain timeout for a reader to reach the end of its stream.
pub(super) async fn finish_reader(mut handle: AbortOnDrop<()>) {
    // Reader failure leaves the partial capture available. On timeout its owner aborts
    // it on drop; the command's status still reports why execution ended.
    let _ = tokio::time::timeout(PIPE_DRAIN_TIMEOUT, &mut handle.0).await;
}

/// Take the captured bytes, leaving the buffer empty.
pub(super) fn take_capture(capture: &Mutex<Captured>) -> Captured {
    std::mem::take(&mut *lock(capture))
}

/// Kill the child if it still runs, and reap it.
pub(super) async fn stop_child(child: &mut Child) {
    // Fails only if the child already exited, which is the goal.
    let _ = child.start_kill();
    // Reap the child so it does not linger as a zombie. If it takes longer than this,
    // `kill_on_drop` still cleans up when `child` is dropped.
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
}
