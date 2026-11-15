//! `zx-agent run`: one task to completion without a person; the exit code reports the outcome.

use std::io::{IsTerminal, Read};
use std::path::Path;

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;

use crate::config::Builtin;
use zx_agent::ai::Message;
use zx_agent::events::{Event, EventKind, RunOutcome, RunReport};
use zx_agent::{Run, Session};

use crate::assembly;
use crate::assembly::SessionPlace;
use crate::cli::{self, OutputMode, RunArgs};
use crate::process::{
    EXIT_DENIED, JsonlEvents, exit_code, next_event, print_line, run_traces, spawn_signal_handler, with_telemetry,
};
use crate::render::{answer_end_line, progress_line, visible};

/// Run one task and return its exit code. An `Err` is a configuration problem found
/// before the run started.
pub(crate) async fn run(args: RunArgs) -> Result<u8> {
    let RunArgs {
        prompt,
        fail_on_denied,
        session: session_file,
        base,
        budget,
        output,
        deltas,
        ..
    } = args;
    let prompt = match &prompt {
        Some(prompt) => Some(read_prompt(prompt)?),
        None => {
            // Checked before anything starts: continuing a new session has nothing to answer.
            let file = session_file.as_deref().context("--continue needs --session")?;
            anyhow::ensure!(
                file.exists(),
                "--continue needs an existing session file; {} does not exist",
                file.display()
            );
            None
        }
    };
    with_telemetry(async move |telemetry| {
        let shared = assembly::Shared::connect(cli::overrides(base, &budget), Builtin::Unattended)
            .await?
            .with_traces(telemetry.map(run_traces));
        let session = build_session(&shared, session_file.as_deref()).await?;
        let cancel = CancellationToken::new();
        spawn_signal_handler(cancel.clone());
        let run = match prompt {
            Some(prompt) => session.send(prompt)?,
            None => session.continue_run()?,
        };
        let report = read_to_end(&session, run, &cancel, &Progress::of(output, deltas)).await;
        print_report(&report, output, session_file.as_deref());
        Ok(match report.outcome {
            RunOutcome::Completed if fail_on_denied && report.denied_calls > 0 => EXIT_DENIED,
            outcome => exit_code(outcome),
        })
    })
    .await
}

fn read_prompt(arg: &str) -> Result<String> {
    let prompt = if arg == "-" {
        // A startup read: the session and the runtime's other tasks do not exist yet, so
        // nothing waits while stdin is read to its end.
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .context("cannot read the prompt from stdin")?;
        text
    } else {
        arg.to_string()
    };
    anyhow::ensure!(!prompt.trim().is_empty(), "the prompt is empty");
    Ok(prompt)
}

/// A session of the `unattended` agent, or of the user's file that replaces it, in which
/// nobody answers approvals: `--session` continues the session its file holds, or starts
/// the file.
async fn build_session(shared: &assembly::Shared, session: Option<&Path>) -> Result<Session> {
    let place = SessionPlace::file_or_memory(session);
    let held = place.held(shared.overrides.cwd.as_deref()).await?;
    let agent = assembly::ModeAgent::build(shared, &shared.settings.model, &place, assembly::Approvals::Nobody)?;
    match held {
        Some(id) => agent.resume(&id).await,
        None => agent.create(None).await,
    }
}

/// Read `run` to its end, showing its events as `progress` does. A signal, which cancels
/// `cancel`, cancels the run through `session`; the run is still read to its end, for its
/// report. A signal that arrived before the run started cancels it at once.
async fn read_to_end(session: &Session, mut run: Run, cancel: &CancellationToken, progress: &Progress) -> RunReport {
    while let Some(event) = next_event(&mut run, session, cancel).await {
        progress.emit(&event);
    }
    run.wait().await
}

/// The answer on stdout and a summary line on stderr, which names `session`, the session
/// file, when there is one.
fn print_report(report: &RunReport, output: OutputMode, session: Option<&Path>) {
    match output {
        OutputMode::Text => {
            if !report.final_text.is_empty() {
                // A terminal would run control sequences in the answer; a pipe gets the exact text.
                if std::io::stdout().is_terminal() {
                    print_line(&visible(&report.final_text));
                } else {
                    print_line(&report.final_text);
                }
            }
            let usage = &report.usage;
            let error = report
                .error
                .as_deref()
                .map(|e| format!(" error={}", visible(e)))
                .unwrap_or_default();
            let denied = match report.denied_calls {
                0 => String::new(),
                count => format!(" denied_calls={count}"),
            };
            let cost = usage
                .total_cost
                .map(|cost| format!(" cost={cost:.4}"))
                .unwrap_or_default();
            let session = session
                .map(|file| format!(" session={}", file.display()))
                .unwrap_or_default();
            // Named as the `run_end` event names them.
            let outcome = match report.outcome {
                RunOutcome::BudgetExhausted { budget, truncated } => {
                    format!("budget_exhausted budget={} truncated={truncated}", budget.name())
                }
                other => other.name().to_string(),
            };
            eprintln!(
                "[zx-agent] outcome={outcome} model_requests={} retries={} tokens={} (in {} / out {}){cost}{denied}{error}{session}",
                report.model_requests, report.retries, usage.total_tokens, usage.input, usage.output,
            );
        }
        // The `run_end` event, already written, carries the report.
        OutputMode::Jsonl => {}
    }
}

/// How a run shows its events while it runs.
enum Progress {
    /// Short progress lines on stderr, printed like chat's; the answer is printed with the
    /// report.
    Text,
    /// Every event as one JSON line on stdout.
    Jsonl(JsonlEvents),
}

impl Progress {
    fn of(output: OutputMode, deltas: bool) -> Self {
        match output {
            OutputMode::Text => Self::Text,
            OutputMode::Jsonl => Self::Jsonl(JsonlEvents { deltas }),
        }
    }

    fn emit(&self, event: &Event) {
        match self {
            Self::Text => text_progress(event),
            Self::Jsonl(events) => events.emit(event),
        }
    }
}

/// The line that `event` adds to the progress on stderr, if it adds one.
fn text_progress(event: &Event) {
    if let EventKind::MessageEnd {
        message: Message::Assistant(message),
        ..
    } = &event.event
    {
        if let Some((_, line)) = answer_end_line(message) {
            eprintln!("{line}");
        }
    } else if let Some((_, line)) = progress_line(&event.event) {
        eprintln!("{line}");
    }
}
