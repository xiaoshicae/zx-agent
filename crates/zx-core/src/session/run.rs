//! One run of a session as its caller holds it: the events it emits, and its result.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::Stream;
use tokio::sync::mpsc;

use crate::events::{Event, RunReport};

/// How many events a run may emit ahead of its reader. A run that is further ahead waits,
/// so a slow reader slows the run down instead of filling memory.
pub(crate) const EVENT_BUFFER: usize = 64;

/// A prompt, a continuation or a compaction in progress: the stream of its events, and its
/// result, `T`.
///
/// The run makes progress only while it is read: [`Run::next`], [`Run::wait`] or polling it
/// as a [`Stream`] is what runs it. Its events arrive in the order of their `seq`. Dropping
/// it cancels the run and releases the session at once; a host that wants the run to go on
/// after its caller has left moves it into a task of its own.
///
/// A run that is dropped while it records an entry leaves the session closed, because
/// whether the storage got that entry is unknown: [`Session`](super::Session) says how it is
/// opened again. A host that cancels with [`Session::cancel`](super::Session::cancel) and
/// reads the run to its end never meets this.
///
/// ```no_run
/// use zx_core::session::{Session, SessionError};
///
/// # async fn example(session: Session) -> Result<(), SessionError> {
/// let mut run = session.send("Inspect the logs.")?;
/// while let Some(event) = run.next().await {
///     println!("#{} {}", event.seq, event.event.kind());
/// }
/// let report = run.wait().await;
/// println!("{:?}: {}", report.outcome, report.final_text);
/// # Ok(())
/// # }
/// ```
#[must_use = "a run makes no progress until it is read or waited for"]
pub struct Run<T = RunReport> {
    id: String,
    events: mpsc::Receiver<Event>,
    progress: Progress<T>,
}

/// How far a run is.
enum Progress<T> {
    /// The run itself, which holds the session until it ends or is dropped, and the only
    /// sender of its events.
    Running(Pin<Box<dyn Future<Output = T> + Send>>),
    Ended(T),
}

// The result is only ever moved, never pinned in place.
impl<T> Unpin for Run<T> {}

impl<T> Run<T> {
    /// The run `id` whose events arrive at `events` while `work` runs. `work` holds the only
    /// sender, so the events end when it does.
    pub(crate) fn new(
        id: String,
        events: mpsc::Receiver<Event>,
        work: impl Future<Output = T> + Send + 'static,
    ) -> Self {
        Self {
            id,
            events,
            progress: Progress::Running(Box::pin(work)),
        }
    }

    /// The run's ID, as in the `runId` of its events.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The next event; `None` once the run has ended and its events are all read. Reading
    /// is what runs the run. An event is taken only when it is returned, so dropping this
    /// future, as `select!` does, loses none.
    pub async fn next(&mut self) -> Option<Event> {
        std::future::poll_fn(|cx| Pin::new(&mut *self).poll_next(cx)).await
    }

    /// Run to the end and return the result. Events not read before are dropped.
    pub async fn wait(mut self) -> T {
        while self.next().await.is_some() {}
        match self.progress {
            Progress::Ended(result) => result,
            // Not reached: the events end only once the run has. Waiting for it is right
            // all the same.
            Progress::Running(work) => work.await,
        }
    }
}

impl<T> Stream for Run<T> {
    type Item = Event;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Event>> {
        let run = self.get_mut();
        loop {
            // What the run has emitted comes first: the run waits for its reader, and the
            // events end once the run has, when its sender is gone.
            if let Poll::Ready(next) = run.events.poll_recv(cx) {
                return Poll::Ready(next);
            }
            let Progress::Running(work) = &mut run.progress else {
                return Poll::Pending;
            };
            match work.as_mut().poll(cx) {
                // Dropping the run's future here closes the events, which the next round
                // reads to their end.
                Poll::Ready(result) => run.progress = Progress::Ended(result),
                // The run may have emitted events before it had to wait.
                Poll::Pending => return run.events.poll_recv(cx),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use futures::FutureExt;

    use super::*;
    use crate::events::{EVENT_SCHEMA_VERSION, EventKind};

    fn event(seq: u64) -> Event {
        Event {
            schema_version: EVENT_SCHEMA_VERSION,
            seq,
            at_ms: 0,
            session_id: "session".into(),
            run_id: "run".into(),
            event: EventKind::StepStart { step: 1 },
        }
    }

    /// A run that emits `count` events and returns how many it emitted.
    fn emitting(count: u64, emitted: Arc<AtomicUsize>) -> Run<u64> {
        let (sender, receiver) = mpsc::channel(EVENT_BUFFER);
        Run::new("run".into(), receiver, async move {
            for seq in 1..=count {
                sender.send(event(seq)).await.unwrap();
                emitted.fetch_add(1, Ordering::SeqCst);
            }
            count
        })
    }

    #[tokio::test]
    async fn events_arrive_in_order_and_the_result_after_them() {
        let mut run = emitting(3, Arc::default());
        assert_eq!(run.id(), "run");

        let mut seqs = Vec::new();
        while let Some(event) = run.next().await {
            seqs.push(event.seq);
        }

        assert_eq!(seqs, [1, 2, 3]);
        assert_eq!(run.wait().await, 3);
    }

    #[tokio::test]
    async fn a_run_makes_no_progress_until_it_is_read() {
        let emitted = Arc::new(AtomicUsize::new(0));
        let run = emitting(3, emitted.clone());
        tokio::task::yield_now().await;
        assert_eq!(emitted.load(Ordering::SeqCst), 0);

        assert_eq!(run.wait().await, 3);
        assert_eq!(emitted.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_run_ahead_of_its_reader_waits_for_it() {
        let emitted = Arc::new(AtomicUsize::new(0));
        let total = u64::try_from(EVENT_BUFFER).unwrap() * 3;
        let mut run = emitting(total, emitted.clone());

        assert_eq!(run.next().await.map(|event| event.seq), Some(1));
        // One read lets the run fill the buffer again, and no more.
        assert!(emitted.load(Ordering::SeqCst) <= EVENT_BUFFER + 1);

        let mut read = 1;
        while run.next().await.is_some() {
            read += 1;
        }
        assert_eq!(read, total);
    }

    #[tokio::test]
    async fn dropping_a_run_drops_its_work() {
        struct Flag(Arc<AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = Flag(dropped.clone());
        let (sender, receiver) = mpsc::channel(EVENT_BUFFER);
        let mut run = Run::new("run".into(), receiver, async move {
            let _held = (flag, sender);
            std::future::pending::<()>().await;
        });
        assert!(run.next().now_or_never().is_none(), "the run is waiting");

        drop(run);

        assert!(dropped.load(Ordering::SeqCst));
    }
}
