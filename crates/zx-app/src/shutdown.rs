//! How a host's process ends: SIGINT and SIGTERM cancel what runs, which is still read to its
//! end for its report, a second signal ends the process at once, and the trace export sends
//! what remains before the host exits. Nothing here prints: what a host may show is returned.

use std::io;

use anyhow::Result;
#[cfg(unix)]
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio_util::sync::CancellationToken;
use zx_agent::ai::Proxy;
use zx_agent::events::Event;
use zx_agent::{Run, Session};

use crate::otel::Telemetry;

/// Which signal arrived.
#[derive(Clone, Copy)]
pub enum Interrupt {
    /// SIGINT, or Ctrl+C.
    Cancel,
    /// SIGTERM.
    Terminate,
}

/// SIGINT and SIGTERM listeners, registered for as long as this lives: a host that cancels one
/// prompt on a signal keeps listening for the next.
pub struct Signals {
    #[cfg(unix)]
    interrupt: Signal,
    #[cfg(unix)]
    terminate: Signal,
    #[cfg(not(unix))]
    interrupt: tokio::signal::windows::CtrlC,
}

impl Signals {
    /// Listen for SIGINT and SIGTERM. Needs a Tokio runtime; fails when a listener cannot be
    /// registered.
    pub fn new() -> io::Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self {
                interrupt: signal(SignalKind::interrupt())?,
                terminate: signal(SignalKind::terminate())?,
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                interrupt: tokio::signal::windows::ctrl_c()?,
            })
        }
    }

    /// The next signal. `recv` yields `None` only when the runtime shuts down, which ends the
    /// process anyway.
    pub async fn next(&mut self) -> Interrupt {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.interrupt.recv() => Interrupt::Cancel,
                _ = self.terminate.recv() => Interrupt::Terminate,
            }
        }
        #[cfg(not(unix))]
        {
            match self.interrupt.recv().await {
                Some(()) => Interrupt::Cancel,
                None => Interrupt::Terminate,
            }
        }
    }
}

/// SIGINT and SIGTERM cancel `cancel`; what it cancels still reports how it ended. On Unix a
/// second signal ends the process at once, for a stop that takes too long, such as an export
/// to a collector that does not answer.
///
/// The listeners are registered before this returns, so a signal that arrives once the work
/// has started always cancels it instead of killing the process. The waiting task is detached
/// on purpose: it lives until a signal ends the process, or the process exits.
///
/// Fails when the listeners cannot be registered: no signal then cancels `cancel`, and the
/// host decides whether to go on without.
pub fn spawn_signal_handler(cancel: CancellationToken) -> io::Result<()> {
    let mut signals = Signals::new()?;
    tokio::spawn(async move {
        signals.next().await;
        cancel.cancel();
        #[cfg(unix)]
        end_by(signals.next().await);
    });
    Ok(())
}

/// End the process at once by the signal of `interrupt`, as it ends without a listener:
/// shells and supervisors see a death by that signal. Tokio goes on catching a signal it has
/// listened for, also once no listener is left, so the default action is restored first.
#[cfg(unix)]
fn end_by(interrupt: Interrupt) {
    let number = match interrupt {
        Interrupt::Cancel => libc::SIGINT,
        Interrupt::Terminate => libc::SIGTERM,
    };
    // SAFETY: `signal` and `raise` take plain integers and touch no memory of this program;
    // with the default action back, the raised signal ends the process.
    unsafe {
        libc::signal(number, libc::SIG_DFL);
        libc::raise(number);
    }
}

/// The next event of `run`, a run of `session`. Once `stop` is cancelled, as a signal
/// cancels it, so is the run, which is still read to its end for its report; cancelling a
/// cancelled run again changes nothing.
pub async fn next_event<T>(run: &mut Run<T>, session: &Session, stop: &CancellationToken) -> Option<Event> {
    tokio::select! {
        biased;
        () = stop.cancelled() => {
            session.cancel();
            run.next().await
        }
        event = run.next() => event,
    }
}

/// What a host says as its work starts with the trace export on: [`with_telemetry`] gave the
/// work an export.
pub const EXPORT_NOTICE: &str = "[otel] exporting traces over OTLP/HTTP";

/// How the trace export ended, for the host to report: nothing when nothing was exported, or
/// all of it was.
#[derive(Debug, Default)]
pub struct ExportEnd {
    /// Events that were not exported: the queue was full, or the export had stopped.
    pub dropped: u64,
    /// Why the spans that remained could not all be sent, when they could not.
    pub error: Option<anyhow::Error>,
}

impl ExportEnd {
    /// What a host says of how the export ended, a line each: the events that were not
    /// exported, and why the rest could not all be sent, which may quote the collector.
    /// Nothing when all of it was.
    pub fn notices(&self) -> Vec<String> {
        let mut notices = Vec::new();
        if self.dropped > 0 {
            notices.push(format!(
                "[otel] {} event(s) were not exported: the queue was full, or the export had stopped",
                self.dropped
            ));
        }
        if let Some(error) = &self.error {
            notices.push(format!("[otel] {error:#}"));
        }
        notices
    }
}

/// Run `body` with the trace export started, when the environment names an OTLP endpoint, as
/// the service `service_name`, the host's name, sending through `proxy`, config.toml's
/// ([`Telemetry::from_env`]); once `body` returns,
/// whatever it returns, what remains to export is sent before this returns. `body` gets the
/// export, `None` when nothing is exported, to observe the agents' runs with its hook
/// ([`Application::set_traces`](crate::Application::set_traces)).
///
/// Fails before `body` starts when the exporter refuses its configuration, such as an invalid
/// endpoint. The export never changes what `body` returns: how it ended comes beside it.
pub async fn with_telemetry<B, T>(service_name: &str, proxy: &Proxy, body: B) -> Result<(T, ExportEnd)>
where
    B: AsyncFnOnce(Option<&Telemetry>) -> T,
{
    let telemetry = Telemetry::from_env(service_name, proxy)?;
    let value = body(telemetry.as_ref()).await;
    let end = match telemetry {
        Some(telemetry) => finish(telemetry).await,
        None => ExportEnd::default(),
    };
    Ok((value, end))
}

/// End the export of `telemetry`: what it dropped, and whether the rest went out.
async fn finish(telemetry: Telemetry) -> ExportEnd {
    let dropped = telemetry.dropped();
    ExportEnd {
        dropped,
        error: telemetry.shutdown().await.err(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_export_that_lost_nothing_has_nothing_to_say() {
        assert!(ExportEnd::default().notices().is_empty());
    }

    #[test]
    fn an_export_that_lost_events_says_how_many_and_why_the_rest_failed() {
        let end = ExportEnd {
            dropped: 2,
            error: Some(anyhow::anyhow!("refused").context("cannot export")),
        };
        assert_eq!(
            end.notices(),
            [
                "[otel] 2 event(s) were not exported: the queue was full, or the export had stopped",
                "[otel] cannot export: refused",
            ]
        );
    }
}
