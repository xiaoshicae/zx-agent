//! A channel under test, running until the test stops it.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use zx_agent::im::{ImChannel, ImError, Inbound};

/// A running channel: what it passes on, and how to stop it.
pub(crate) struct Running {
    inbound: mpsc::Receiver<Inbound>,
    wait: Duration,
    cancel: CancellationToken,
    task: JoinHandle<Result<(), ImError>>,
}

/// Run `channel` in its own task; `next` waits up to `wait` for each event.
pub(crate) fn run_channel<C: ImChannel + ?Sized + 'static>(channel: Arc<C>, wait: Duration) -> Running {
    let (sender, inbound) = mpsc::channel(16);
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    let task = tokio::spawn(async move { channel.run(sender, &stop).await });
    Running {
        inbound,
        wait,
        cancel,
        task,
    }
}

impl Running {
    /// The next event the channel passed on; a test fails when none comes in time.
    pub(crate) async fn next(&mut self) -> Inbound {
        tokio::time::timeout(self.wait, self.inbound.recv())
            .await
            .expect("nothing was passed on in time")
            .expect("the channel is running")
    }

    /// Cancel the channel and return how its run ended.
    pub(crate) async fn stop(self) -> Result<(), ImError> {
        self.cancel.cancel();
        self.ended().await
    }

    /// How the run ended, for a channel that stops on its own.
    pub(crate) async fn ended(self) -> Result<(), ImError> {
        self.task.await.unwrap()
    }
}
