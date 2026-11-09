//! What every subcommand shares of the process: exit codes, lines on stdout, the shutdown
//! signals and the telemetry export.

use std::io::Write;
use std::sync::Arc;

use anyhow::Result;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

use zx_agent::agent::{AgentEvent, Event, EventSink};
use zx_agent::otel::{Telemetry, TraceParent};
use zx_agent::runner::RunOutcome;

/// Exit codes, so schedulers can react without parsing output.
pub(crate) const EXIT_COMPLETED: u8 = 0;
pub(crate) const EXIT_ERROR: u8 = 1;
pub(crate) const EXIT_USAGE: u8 = 2;
const EXIT_MODEL_REQUEST_LIMIT: u8 = 3;
const EXIT_TIMEOUT: u8 = 4;
pub(crate) const EXIT_ABORTED: u8 = 5;
const EXIT_TRUNCATED: u8 = 6;
const EXIT_TOKEN_BUDGET: u8 = 7;
const EXIT_COST_BUDGET: u8 = 8;
/// `run --fail-on-denied`: completed, but a policy or an approval denied a tool call.
pub(crate) const EXIT_DENIED: u8 = 9;

/// The exit code that reports `outcome`.
pub(crate) fn exit_code(outcome: RunOutcome) -> u8 {
    match outcome {
        RunOutcome::Completed => EXIT_COMPLETED,
        RunOutcome::Error => EXIT_ERROR,
        RunOutcome::ModelRequestLimit => EXIT_MODEL_REQUEST_LIMIT,
        RunOutcome::Timeout => EXIT_TIMEOUT,
        RunOutcome::Aborted => EXIT_ABORTED,
        RunOutcome::Truncated => EXIT_TRUNCATED,
        RunOutcome::TokenBudget => EXIT_TOKEN_BUDGET,
        RunOutcome::CostBudget => EXIT_COST_BUDGET,
    }
}

/// SIGINT and SIGTERM cancel the run; the report is still written.
///
/// The listeners are registered before this returns, so a signal that arrives once the run
/// has started always cancels it instead of killing the process. The waiting task is detached
/// on purpose: it lives until a signal arrives or the process exits.
pub(crate) fn spawn_signal_handler(cancel: CancellationToken) {
    match shutdown_signal() {
        Ok(signal) => {
            tokio::spawn(async move {
                signal.await;
                cancel.cancel();
            });
        }
        // Without signal handling the run is still bounded by its own limits, so it continues.
        Err(e) => eprintln!("zx-agent: cannot listen for shutdown signals: {e}"),
    }
}

/// Register SIGINT and SIGTERM listeners; the future completes at the first signal.
#[cfg(unix)]
fn shutdown_signal() -> std::io::Result<impl std::future::Future<Output = ()>> {
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    Ok(async move {
        // `recv` yields `None` only when the runtime shuts down, which ends the process anyway.
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
        }
    })
}

/// Register a Ctrl+C listener; the future completes at the first Ctrl+C.
#[cfg(not(unix))]
fn shutdown_signal() -> std::io::Result<impl std::future::Future<Output = ()>> {
    let mut ctrl_c = tokio::signal::windows::ctrl_c()?;
    Ok(async move {
        // `recv` yields `None` only when the runtime shuts down, which ends the process anyway.
        ctrl_c.recv().await;
    })
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

impl EventSink for JsonlEvents {
    fn emit(&self, event: &Event) {
        if !self.deltas && matches!(event.event, AgentEvent::MessageUpdate { .. }) {
            return;
        }
        write_line(event);
    }
}

/// Run `mode` with the trace and metric export started, when an OTLP endpoint is configured,
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

/// Export traces and metrics when an OTLP endpoint is configured (`zx_agent::otel`), and say
/// so on stderr.
fn start_telemetry() -> Result<Option<Telemetry>> {
    let telemetry = Telemetry::from_env()?;
    if let Some(telemetry) = &telemetry {
        eprintln!("[otel] exporting {} over OTLP/HTTP", telemetry.exports());
    }
    Ok(telemetry)
}

/// The sink for an unattended run: the run continues the caller's trace when `TRACEPARENT`
/// names one, as a scheduler or `otel-cli` sets it. A value that is not a traceparent is
/// reported, and the run starts a trace of its own.
pub(crate) fn run_sink(telemetry: &Telemetry) -> Arc<dyn EventSink> {
    match TraceParent::from_env() {
        Ok(Some(parent)) => {
            eprintln!("[otel] continuing trace {}", parent.trace_id());
            telemetry.sink_under(&parent)
        }
        Ok(None) => telemetry.sink(),
        Err(error) => {
            eprintln!("[otel] {error:#}; the run starts a trace of its own");
            telemetry.sink()
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
