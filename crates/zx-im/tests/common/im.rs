//! A channel under test, running until the test stops it, a backlog that cannot keep
//! anything, and where a message is.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use zx_im::{
    Backlog, ConversationId, Destination, ImChannel, ImError, Inbound, Inlet, Kept, MemoryBacklog, MessageKey, ThreadId,
};

/// A running channel: what it passes on, and how to stop it.
pub(crate) struct Running {
    inbound: mpsc::Receiver<Inbound>,
    wait: Duration,
    cancel: CancellationToken,
    task: JoinHandle<Result<(), ImError>>,
}

/// Run `channel` in its own task, keeping its messages in a backlog of its own; `next` waits
/// up to `wait` for each event.
pub(crate) fn run_channel<C: ImChannel + ?Sized + 'static>(channel: Arc<C>, wait: Duration) -> Running {
    run_channel_with(channel, Arc::new(MemoryBacklog::default()), wait)
}

/// Run `channel` as [`run_channel`] does, keeping its messages in `backlog`.
pub(crate) fn run_channel_with<C: ImChannel + ?Sized + 'static>(
    channel: Arc<C>,
    backlog: Arc<dyn Backlog>,
    wait: Duration,
) -> Running {
    let (sender, inbound) = mpsc::channel(16);
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    let inlet = Inlet::new(channel.platform(), sender, backlog);
    let task = tokio::spawn(async move { channel.run(inlet, &stop).await });
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

    /// Whether the channel has passed nothing on that the test has not read.
    pub(crate) fn nothing_passed(&mut self) -> bool {
        self.inbound.try_recv().is_err()
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

/// A backlog whose volume refuses every write.
pub(crate) struct FullBacklog;

#[async_trait]
impl Backlog for FullBacklog {
    async fn keep(&self, _key: &MessageKey, _record: &str) -> Result<Kept, String> {
        Err("no space left on device".into())
    }

    async fn complete(&self, _key: &MessageKey) -> Result<(), String> {
        Ok(())
    }

    async fn pending(&self) -> Result<Vec<(MessageKey, String)>, String> {
        Ok(Vec::new())
    }
}

/// Where a message is: in `conversation`, in the thread under `root` when there is one.
pub(crate) fn at(conversation: &str, root: Option<&str>) -> Destination {
    Destination {
        conversation: ConversationId(conversation.into()),
        thread: root.map(|root| ThreadId(root.into())),
    }
}
