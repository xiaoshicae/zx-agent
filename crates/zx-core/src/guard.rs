//! What guards the kernel's calls into host code: the run's cancellation as that code sees
//! it, and the bound that the calls into hooks, policies and summarizers get.

use std::fmt;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::time::Duration;

use futures::FutureExt;
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};

/// A run's cancellation as host code sees it: a hook or a policy can wait for it, check it,
/// and derive a token for work of its own, but cannot cancel the run, which only its caller
/// does.
///
/// ```compile_fail
/// fn stop(ctx: &zx_core::hooks::HookContext) {
///     ctx.cancel.cancel();
/// }
/// ```
#[derive(Clone, Debug)]
pub struct CancelSignal(CancellationToken);

impl CancelSignal {
    pub(crate) fn new(token: CancellationToken) -> Self {
        Self(token)
    }

    /// Whether the run has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }

    /// Completes once the run is cancelled.
    pub fn cancelled(&self) -> WaitForCancellationFuture<'_> {
        self.0.cancelled()
    }

    /// A token cancelled with the run, for work of the host code's own; cancelling it stops
    /// only that work.
    pub fn child_token(&self) -> CancellationToken {
        self.0.child_token()
    }

    /// The run's own token, for the kernel, which waits on the host code.
    pub(crate) fn token(&self) -> &CancellationToken {
        &self.0
    }
}

/// Why a bounded call ended without its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stopped {
    /// The call did not finish within this time.
    TimedOut(Duration),
    Panicked,
    /// The run was cancelled while the call ran.
    Cancelled,
}

impl fmt::Display for Stopped {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimedOut(timeout) => write!(formatter, "timed out after {}ms", timeout.as_millis()),
            Self::Panicked => formatter.write_str("panicked"),
            Self::Cancelled => formatter.write_str("was cancelled"),
        }
    }
}

/// Which counts when a call is ready and the run is cancelled at the same moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Priority {
    /// The cancellation: the call's value is dropped. For calls that decide or change
    /// something, which a cancelled run must not act on.
    Cancellation,
    /// The call's value. For observers, which still see the last events of a cancelled run.
    Completion,
}

/// Wait for `future`, bounded by `timeout` and by `cancel`. A panic, a timeout and a
/// cancellation each end the wait with their own [`Stopped`].
pub(crate) async fn bounded<T>(
    future: impl Future<Output = T>,
    timeout: Duration,
    cancel: &CancellationToken,
    priority: Priority,
) -> Result<T, Stopped> {
    let guarded = tokio::time::timeout(timeout, AssertUnwindSafe(future).catch_unwind());
    tokio::pin!(guarded);
    let result = match priority {
        Priority::Completion => tokio::select! {
            biased;
            result = &mut guarded => Some(result),
            () = cancel.cancelled() => None,
        },
        Priority::Cancellation => tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            result = &mut guarded => Some(result),
        },
    };
    match result {
        Some(Ok(Ok(value))) => Ok(value),
        Some(Ok(Err(_))) => Err(Stopped::Panicked),
        Some(Err(_)) => Err(Stopped::TimedOut(timeout)),
        None => Err(Stopped::Cancelled),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LONG: Duration = Duration::from_secs(60);

    #[tokio::test]
    async fn a_finished_call_gives_its_value() {
        let value = bounded(async { 7 }, LONG, &CancellationToken::new(), Priority::Cancellation).await;
        assert_eq!(value, Ok(7));
    }

    #[tokio::test]
    async fn a_panic_is_caught() {
        let stopped = bounded(
            async { panic!("host bug") },
            LONG,
            &CancellationToken::new(),
            Priority::Cancellation,
        )
        .await;
        assert_eq!(stopped, Err::<(), _>(Stopped::Panicked));
    }

    #[tokio::test]
    async fn a_pending_call_is_bounded_by_its_timeout() {
        let timeout = Duration::from_millis(1);
        let stopped = bounded(
            std::future::pending::<()>(),
            timeout,
            &CancellationToken::new(),
            Priority::Cancellation,
        )
        .await;
        assert_eq!(stopped, Err(Stopped::TimedOut(timeout)));
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_pending_call_before_its_timeout() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let stopped = bounded(std::future::pending::<()>(), LONG, &cancel, Priority::Cancellation).await;
        assert_eq!(stopped, Err(Stopped::Cancelled));
    }

    #[tokio::test]
    async fn a_ready_call_and_a_cancelled_run_follow_the_priority() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            bounded(async { 1 }, LONG, &cancel, Priority::Cancellation).await,
            Err(Stopped::Cancelled)
        );
        assert_eq!(bounded(async { 1 }, LONG, &cancel, Priority::Completion).await, Ok(1));
    }

    #[test]
    fn a_stop_reads_as_the_end_of_a_sentence_about_the_call() {
        assert_eq!(
            Stopped::TimedOut(Duration::from_millis(250)).to_string(),
            "timed out after 250ms"
        );
        assert_eq!(Stopped::Panicked.to_string(), "panicked");
        assert_eq!(Stopped::Cancelled.to_string(), "was cancelled");
    }
}
