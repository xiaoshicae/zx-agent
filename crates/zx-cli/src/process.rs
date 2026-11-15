//! What every subcommand shares of the process: exit codes, lines on stdout, the shutdown
//! signals and the telemetry export.

use std::io::{self, Write};

use anyhow::Result;
#[cfg(unix)]
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio_util::sync::CancellationToken;

use crate::otel::{Telemetry, TraceHook, TraceParent};
use zx_agent::events::{Budget, Event, EventKind, RunOutcome};
use zx_agent::{Run, Session};

/// Exit codes, so schedulers can react without parsing output.
pub(crate) const EXIT_COMPLETED: u8 = 0;
pub(crate) const EXIT_ERROR: u8 = 1;
pub(crate) const EXIT_USAGE: u8 = 2;
const EXIT_MODEL_REQUEST_LIMIT: u8 = 3;
const EXIT_TIMEOUT: u8 = 4;
pub(crate) const EXIT_CANCELLED: u8 = 5;
const EXIT_TRUNCATED: u8 = 6;
const EXIT_TOKEN_BUDGET: u8 = 7;
const EXIT_COST_BUDGET: u8 = 8;
/// `run --fail-on-denied`: completed, but a policy or an approval denied a tool call.
pub(crate) const EXIT_DENIED: u8 = 9;

/// The exit code that reports `outcome`.
pub(crate) fn exit_code(outcome: RunOutcome) -> u8 {
    match outcome {
        RunOutcome::Completed => EXIT_COMPLETED,
        RunOutcome::Failed => EXIT_ERROR,
        RunOutcome::BudgetExhausted { budget, .. } => match budget {
            Budget::ModelRequests => EXIT_MODEL_REQUEST_LIMIT,
            Budget::Tokens => EXIT_TOKEN_BUDGET,
            Budget::Cost => EXIT_COST_BUDGET,
        },
        RunOutcome::TimedOut => EXIT_TIMEOUT,
        RunOutcome::Cancelled => EXIT_CANCELLED,
        RunOutcome::Truncated => EXIT_TRUNCATED,
    }
}

/// SIGINT and SIGTERM cancel `cancel`; what it cancels still reports how it ended.
///
/// The listeners are registered before this returns, so a signal that arrives once the run
/// has started always cancels it instead of killing the process. The waiting task is detached
/// on purpose: it lives until a signal arrives or the process exits.
pub(crate) fn spawn_signal_handler(cancel: CancellationToken) {
    match Signals::new() {
        Ok(mut signals) => {
            tokio::spawn(async move {
                signals.next().await;
                cancel.cancel();
            });
        }
        // Without signal handling the run is still bounded by its own limits, so it continues.
        Err(e) => eprintln!("zx-agent: cannot listen for shutdown signals: {e}"),
    }
}

/// The next event of `run`, a run of `session`. Once `stop` is cancelled, as a signal
/// cancels it, so is the run, which is still read to its end for its report; cancelling a
/// cancelled run again changes nothing.
pub(crate) async fn next_event(run: &mut Run, session: &Session, stop: &CancellationToken) -> Option<Event> {
    tokio::select! {
        biased;
        () = stop.cancelled() => {
            session.cancel();
            run.next().await
        }
        event = run.next() => event,
    }
}

/// Which signal arrived.
pub(crate) enum Interrupt {
    /// SIGINT, or Ctrl+C.
    Cancel,
    /// SIGTERM.
    Terminate,
}

/// SIGINT and SIGTERM listeners, registered for as long as this lives: a frontend that
/// cancels one prompt on a signal keeps listening for the next.
pub(crate) struct Signals {
    #[cfg(unix)]
    interrupt: Signal,
    #[cfg(unix)]
    terminate: Signal,
    #[cfg(not(unix))]
    interrupt: tokio::signal::windows::CtrlC,
}

impl Signals {
    pub(crate) fn new() -> io::Result<Self> {
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
    pub(crate) async fn next(&mut self) -> Interrupt {
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

/// Print one JSON line on stdout.
pub(crate) fn write_line(value: &impl serde::Serialize) {
    // Events and reports hold only strings, numbers, and `serde_json::Value`, which always serialize.
    let Ok(line) = serde_json::to_string(value) else {
        return;
    };
    print_line(&line);
}

/// Print `text` and a newline on stdout, unlike `println!` without panicking when stdout is
/// closed.
pub(crate) fn print_line(text: &str) {
    let mut out = std::io::stdout().lock();
    // A closed stdout (for example a consumer that exited) leaves nobody to report to; the run
    // continues and the exit code still tells the scheduler how it ended.
    let _ = writeln!(out, "{text}").and_then(|()| out.flush());
}

/// Writes every event as one JSON line on stdout.
pub(crate) struct JsonlEvents {
    pub(crate) deltas: bool,
}

impl JsonlEvents {
    pub(crate) fn emit(&self, event: &Event) {
        if !self.deltas && matches!(event.event, EventKind::MessageUpdate { .. }) {
            return;
        }
        write_line(event);
    }
}

/// Run `mode` with the trace export started, when an OTLP endpoint is configured,
/// and finished once it returns.
///
/// An endpoint the exporter refuses is a configuration error, reported before `mode` starts;
/// what remains to export is sent before the exit code is returned.
pub(crate) async fn with_telemetry<M>(mode: M) -> Result<u8>
where
    M: AsyncFnOnce(Option<&Telemetry>) -> Result<u8>,
{
    let telemetry = start_telemetry()?;
    let result = mode(telemetry.as_ref()).await;
    finish_telemetry(telemetry).await;
    result
}

/// Export traces when an OTLP endpoint is configured (`crate::otel`), and say so on
/// stderr.
fn start_telemetry() -> Result<Option<Telemetry>> {
    let telemetry = Telemetry::from_env()?;
    if telemetry.is_some() {
        eprintln!("[otel] exporting traces over OTLP/HTTP");
    }
    Ok(telemetry)
}

/// The trace hook of an unattended run: the run continues the caller's trace when
/// `TRACEPARENT` names one, as a scheduler or `otel-cli` sets it. A value that is not a
/// traceparent is reported, and the run starts a trace of its own.
pub(crate) fn run_traces(telemetry: &Telemetry) -> TraceHook {
    match TraceParent::from_env() {
        Ok(Some(parent)) => {
            eprintln!("[otel] continuing trace {}", parent.trace_id());
            telemetry.hook_under(&parent)
        }
        Ok(None) => telemetry.hook(),
        Err(error) => {
            eprintln!("[otel] {error:#}; the run starts a trace of its own");
            telemetry.hook()
        }
    }
}

/// Export the spans that remain. Telemetry never changes the outcome: failures only go to
/// stderr.
async fn finish_telemetry(telemetry: Option<Telemetry>) {
    let Some(telemetry) = telemetry else {
        return;
    };
    let dropped = telemetry.dropped();
    if dropped > 0 {
        eprintln!("[otel] {dropped} event(s) were not exported: the queue was full");
    }
    if let Err(error) = telemetry.shutdown().await {
        eprintln!("[otel] {error:#}");
    }
}
