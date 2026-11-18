//! Bounded capture of a child process's output streams, and the end of a child, shared by
//! the executors.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Child;
use tokio_util::sync::CancellationToken;

use super::{Captured, CommandEnd, CommandOutput};
use crate::sync::{AbortOnDrop, lock};

/// Bytes kept per stream while reading. Older output is dropped; only the tail is returned.
const CAPTURE_LIMIT: usize = 1024 * 1024;
/// How long to wait for output pipes after the process ends. Background children that
/// inherited the pipes could otherwise keep them open forever.
const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
/// How long to wait for a killed child to be reaped before leaving it to `kill_on_drop`.
const REAP_TIMEOUT: Duration = Duration::from_secs(5);

/// One output stream being read into a bounded buffer by its own task.
pub(super) struct Capture {
    /// Shared with the task, so what was read survives the task being aborted.
    shared: Arc<Mutex<Captured>>,
    task: Option<AbortOnDrop<()>>,
}

impl Capture {
    /// Start reading `reader`; `None` captures nothing. The task stops at the end of the
    /// stream or on a read error.
    pub(super) fn spawn<R>(reader: Option<R>) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        let shared = Arc::new(Mutex::new(Captured::default()));
        let Some(mut reader) = reader else {
            return Self { shared, task: None };
        };
        let buffer = shared.clone();
        let task = tokio::spawn(async move {
            let mut chunk = [0u8; 8192];
            // A read error ends the capture early; the exit status still reports how the command ended.
            while let Ok(n) = reader.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                let mut capture = lock(&buffer);
                capture.bytes.extend_from_slice(&chunk[..n]);
                if capture.bytes.len() > CAPTURE_LIMIT {
                    let excess = capture.bytes.len() - CAPTURE_LIMIT;
                    capture.bytes.drain(..excess);
                    capture.dropped += excess;
                }
            }
        });
        Self {
            shared,
            task: Some(AbortOnDrop(task)),
        }
    }

    /// Wait up to the drain timeout for the end of the stream, then take what was read.
    pub(super) async fn finish(mut self) -> Captured {
        if let Some(mut task) = self.task.take() {
            // A timed-out reader is aborted when `task` is dropped; what it read so far is
            // kept, and the command's status still reports why execution ended.
            let _ = tokio::time::timeout(PIPE_DRAIN_TIMEOUT, &mut task.0).await;
        }
        std::mem::take(&mut *lock(&self.shared))
    }
}

/// Wait for `child` to end, or for `timeout` or `cancel`, whichever comes first.
pub(super) async fn await_end(child: &mut Child, timeout: Duration, cancel: &CancellationToken) -> CommandEnd {
    tokio::select! {
        status = child.wait() => match status {
            Ok(status) => match status.code() {
                Some(code) => CommandEnd::Exited(code),
                // Also how `docker exec` reports a command ended by signal N: 128 + N.
                None => CommandEnd::Signaled,
            },
            Err(e) => CommandEnd::WaitFailed(e.to_string()),
        },
        () = tokio::time::sleep(timeout) => CommandEnd::TimedOut,
        () = cancel.cancelled() => CommandEnd::Cancelled,
    }
}

/// Stop and reap `child`, then drain both streams into the output.
pub(super) async fn collect(child: &mut Child, stdout: Capture, stderr: Capture, end: CommandEnd) -> CommandOutput {
    stop_child(child).await;
    CommandOutput {
        stdout: stdout.finish().await,
        stderr: stderr.finish().await,
        end,
    }
}

/// Kill the child if it still runs, and reap it.
async fn stop_child(child: &mut Child) {
    // Fails only if the child already exited, which is the goal.
    let _ = child.start_kill();
    // Reap the child so it does not linger as a zombie; past the timeout, `kill_on_drop`
    // still cleans up when `child` is dropped.
    let _ = tokio::time::timeout(REAP_TIMEOUT, child.wait()).await;
}
