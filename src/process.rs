//! What every subcommand shares of the process: exit codes, lines on stdout and the
//! shutdown signals.

use std::io::Write;

#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

use zx_agent::agent::{AgentEvent, Event, EventSink};
use zx_agent::runner::RunOutcome;

/// Exit codes, so schedulers can react without parsing output.
pub(crate) const EXIT_COMPLETED: u8 = 0;
pub(crate) const EXIT_ERROR: u8 = 1;
pub(crate) const EXIT_USAGE: u8 = 2;
pub(crate) const EXIT_MODEL_REQUEST_LIMIT: u8 = 3;
pub(crate) const EXIT_TIMEOUT: u8 = 4;
pub(crate) const EXIT_ABORTED: u8 = 5;
pub(crate) const EXIT_TRUNCATED: u8 = 6;
pub(crate) const EXIT_TOKEN_BUDGET: u8 = 7;

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
