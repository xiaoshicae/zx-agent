//! What every subcommand shares of the process: exit codes, JSON lines on stdout, and what
//! the subcommands say on stderr about the shutdown signals and the trace export, which
//! `zx_app::shutdown` runs.

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use zx_agent::events::{Budget, Event, EventKind, RunOutcome};
use zx_app::otel::Telemetry;
use zx_app::shutdown::{self, spawn_signal_handler};
use zx_util::stdio::{eprint_line, print_line};

use crate::render::visible;

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

/// The traces' `service.name` when the environment names no service: the program's name.
const SERVICE_NAME: &str = "zx-agent";

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

/// SIGINT and SIGTERM cancel `cancel` ([`spawn_signal_handler`]). When they cannot be listened
/// for, the subcommand goes on, still bounded by its own limits, and says so on stderr.
pub(crate) fn cancel_on_signals(cancel: CancellationToken) {
    if let Err(error) = spawn_signal_handler(cancel) {
        eprint_line(&format!("zx-agent: cannot listen for shutdown signals: {error}"));
    }
}

/// Print one JSON line on stdout.
pub(crate) fn print_json_line(value: &impl serde::Serialize) {
    // Events and reports hold only strings, numbers, and `serde_json::Value`, which always serialize.
    let Ok(line) = serde_json::to_string(value) else {
        return;
    };
    print_line(&line);
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
        print_json_line(event);
    }
}

/// Run `body`, a subcommand, with the trace export of [`shutdown::with_telemetry`]: say on
/// stderr that traces are exported when they are, and, once `body` returns, what could not
/// be exported. Telemetry never changes the exit code.
///
/// An endpoint the exporter refuses is a configuration error, reported before `body` starts.
pub(crate) async fn with_telemetry<B>(body: B) -> Result<u8>
where
    B: AsyncFnOnce(Option<&Telemetry>) -> Result<u8>,
{
    let (code, end) = shutdown::with_telemetry(SERVICE_NAME, async move |telemetry: Option<&Telemetry>| {
        if telemetry.is_some() {
            eprint_line(shutdown::EXPORT_NOTICE);
        }
        body(telemetry).await
    })
    .await?;
    // An error quotes the collector's answer.
    for notice in end.notices() {
        eprint_line(&visible(&notice));
    }
    code
}
