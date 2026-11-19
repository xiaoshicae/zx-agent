//! What every subcommand shares of the process: exit codes, the configured application and
//! its sessions, lines on stdout, and what the subcommands say on stderr about the shutdown
//! signals and the trace export, which `zx_app::shutdown` runs.

use std::io::Write;

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use zx_agent::Session;
use zx_agent::events::{Budget, Event, EventKind, RunOutcome};
use zx_agent::transcript::Transcript;
use zx_app::config::Builtin;
use zx_app::otel::Telemetry;
use zx_app::shutdown::{self, spawn_signal_handler};
use zx_app::{Application, ConfiguredAgent, NoModel, Overrides};

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

/// What the process shares, read and connected once for sessions of `builtin`, with its
/// startup notices written to stderr. Without a model, the error names the program's ways to
/// give one.
pub(crate) async fn connect(overrides: Overrides, builtin: Builtin) -> Result<Application> {
    let app = Application::from_environment(overrides, builtin)
        .await
        .map_err(with_model_hint)?;
    for notice in app.notices() {
        eprint_line(notice);
    }
    Ok(app)
}

/// `error`, or, when no model was given, the ways to give one: `--model`, over `ZX_MODEL`,
/// which the application reads, over config.toml.
fn with_model_hint(error: anyhow::Error) -> anyhow::Error {
    match error.downcast_ref::<NoModel>() {
        Some(no_model) => anyhow::anyhow!(
            "no model: pass --model, set ZX_MODEL, or set model in {}",
            no_model.config_file
        ),
        None => error,
    }
}

/// The stored session `id` of `agent`, opened. A cut-short final record, which the next
/// record replaces, is reported on stderr.
pub(crate) async fn open_session(agent: &ConfiguredAgent, id: &str) -> Result<Session> {
    let session = agent.open(id).await?;
    let cut_off = session.with_transcript(Transcript::incomplete_tail_bytes)?;
    if cut_off > 0 {
        eprint_line(&format!(
            "[session] recovered incomplete final record; its {cut_off} bytes are cut off before the next record"
        ));
    }
    Ok(session)
}

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

/// Print `text` and a newline on stdout, unlike `println!` without panicking when stdout is
/// closed.
pub(crate) fn print_line(text: &str) {
    let mut out = std::io::stdout().lock();
    // A closed stdout (for example a consumer that exited) leaves nobody to report to; the run
    // continues and the exit code still tells the scheduler how it ended.
    let _ = writeln!(out, "{text}").and_then(|()| out.flush());
}

/// Print `text` and a newline on stderr, unlike `eprintln!` without panicking when stderr is
/// closed.
pub(crate) fn eprint_line(text: &str) {
    // A closed stderr leaves nobody to tell; the subcommand goes on, and its exit code says
    // how it ended.
    let _ = writeln!(std::io::stderr().lock(), "{text}");
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
            eprint_line("[otel] exporting traces over OTLP/HTTP");
        }
        body(telemetry).await
    })
    .await?;
    if end.dropped > 0 {
        eprint_line(&format!(
            "[otel] {} event(s) were not exported: the queue was full",
            end.dropped
        ));
    }
    if let Some(error) = end.error {
        eprint_line(&format!("[otel] {error:#}"));
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_model_names_the_programs_ways_to_give_one() {
        let no_model = NoModel {
            config_file: "/home/me/.zx-agent/config.toml".into(),
        };
        assert_eq!(
            format!("{:#}", with_model_hint(no_model.into())),
            "no model: pass --model, set ZX_MODEL, or set model in /home/me/.zx-agent/config.toml"
        );
        let other = with_model_hint(anyhow::anyhow!("something else"));
        assert_eq!(format!("{other:#}"), "something else");
    }
}
