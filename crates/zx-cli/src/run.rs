//! `zx-agent run`: one task to completion without a person; the exit code reports the outcome.

use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;

use zx_agent::ai::Message;
use zx_agent::events::{Event, EventKind, RunOutcome, RunReport};
use zx_agent::{Run, Session};
use zx_app::config::Builtin;
use zx_app::otel::{Telemetry, TraceHook, TraceParent};
use zx_app::shutdown::next_event;
use zx_app::{Application, Approvals, SessionPlace};
use zx_util::stdio::{eprint_line, print_line};

use crate::application::{connect, open_or_create};
use crate::cli::{self, OutputMode, RunArgs};
use crate::process::{EXIT_DENIED, JsonlEvents, cancel_on_signals, exit_code, with_telemetry};
use crate::render::{answer_end_line, progress_line, visible};
use crate::session_file;

/// Run one task and return its exit code. An `Err` is a configuration problem found
/// before the run started.
pub(crate) async fn run(args: RunArgs) -> Result<u8> {
    let RunArgs {
        prompt,
        continue_session,
        fail_on_denied,
        session,
        base,
        budget,
        output,
        deltas,
    } = args;
    let task = Task::of(prompt, continue_session, session)?;
    with_telemetry(async move |telemetry| {
        let app = connect(cli::overrides(base, &budget), Builtin::Unattended)
            .await?
            .set_traces(telemetry.map(run_traces));
        let file = task.file().map(Path::to_path_buf);
        let session = build_session(&app, file.as_deref()).await?;
        let cancel = CancellationToken::new();
        cancel_on_signals(cancel.clone());
        let run = match task {
            Task::Prompt { prompt, .. } => session.send(prompt)?,
            Task::Continue { .. } => session.continue_run()?,
        };
        let report = read_to_end(&session, run, &cancel, &Progress::of(output, deltas)).await;
        print_report(&report, output, file.as_deref());
        Ok(match report.outcome {
            RunOutcome::Completed if fail_on_denied && report.denied_calls > 0 => EXIT_DENIED,
            outcome => exit_code(outcome),
        })
    })
    .await
}

/// What a run answers, and the session file it keeps the conversation in.
enum Task {
    /// A prompt, in `--session`'s file when one is given, else in memory.
    Prompt { prompt: String, file: Option<PathBuf> },
    /// `--continue`: what the conversation in `--session`'s file ends with.
    Continue { file: PathBuf },
}

impl Task {
    /// The task that `prompt`, `--continue` and `--session`'s `file` describe, its prompt read.
    /// The parser takes a prompt or `--continue`, not both, and `--continue` only with
    /// `--session`; the last error is for what it refuses.
    fn of(prompt: Option<String>, continue_session: bool, file: Option<PathBuf>) -> Result<Self> {
        match (prompt, continue_session, file) {
            (Some(prompt), false, file) => Ok(Self::Prompt {
                prompt: read_prompt(&prompt)?,
                file,
            }),
            (None, true, Some(file)) => {
                // Checked before anything starts, a synchronous look at one path: continuing a new
                // session has nothing to answer.
                anyhow::ensure!(
                    file.exists(),
                    "--continue needs an existing session file; {} does not exist",
                    file.display()
                );
                Ok(Self::Continue { file })
            }
            _ => anyhow::bail!("give a prompt, or --continue with --session"),
        }
    }

    /// `--session`'s file, when one is given.
    fn file(&self) -> Option<&Path> {
        match self {
            Self::Prompt { file, .. } => file.as_deref(),
            Self::Continue { file } => Some(file),
        }
    }
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

/// The trace hook of an unattended run: the run continues the caller's trace when
/// `TRACEPARENT` names one, as a scheduler or `otel-cli` sets it. A value that is not a
/// traceparent is reported, and the run starts a trace of its own.
fn run_traces(telemetry: &Telemetry) -> TraceHook {
    match TraceParent::from_env() {
        Ok(Some(parent)) => {
            eprint_line(&format!("[otel] continuing trace {}", parent.trace_id()));
            telemetry.hook_under(&parent)
        }
        Ok(None) => telemetry.hook(),
        Err(error) => {
            eprint_line(&format!(
                "[otel] {}; the run starts a trace of its own",
                visible(&format!("{error:#}"))
            ));
            telemetry.hook()
        }
    }
}

/// A session of the `unattended` agent, or of the user's file that replaces it, in which
/// nobody answers approvals: `--session`'s `file` continues the session it holds, or starts;
/// without it the session is kept in memory.
async fn build_session(app: &Application, file: Option<&Path>) -> Result<Session> {
    let (place, held) = match file {
        Some(path) => session_file::open(path, app.cwd()).await?,
        None => (SessionPlace::memory(), None),
    };
    let agent = app.build(app.model(), &place, Approvals::Nobody)?;
    open_or_create(&agent, held.as_deref()).await
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

/// The answer on stdout and a summary line on stderr, which names `file`, the session file,
/// when there is one.
fn print_report(report: &RunReport, output: OutputMode, file: Option<&Path>) {
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
                .map(|error| format!(" error={}", visible(error)))
                .unwrap_or_default();
            let denied = match report.denied_calls {
                0 => String::new(),
                count => format!(" denied_calls={count}"),
            };
            let cost = usage
                .total_cost
                .map(|cost| format!(" cost={cost:.4}"))
                .unwrap_or_default();
            let session = file
                .map(|path| format!(" session={}", path.display()))
                .unwrap_or_default();
            // Named as the `run_end` event names them.
            let outcome = match report.outcome {
                RunOutcome::BudgetExhausted { budget, truncated } => {
                    format!("budget_exhausted budget={} truncated={truncated}", budget.name())
                }
                other => other.name().to_string(),
            };
            eprint_line(&format!(
                "[zx-agent] outcome={outcome} model_requests={} retries={} tokens={} (in {} / out {}){cost}{denied}{error}{session}",
                report.model_requests, report.retries, usage.total_tokens, usage.input, usage.output,
            ));
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
            Self::Text => print_progress(event),
            Self::Jsonl(events) => events.emit(event),
        }
    }
}

/// Print the line that `event` adds to the progress on stderr, if it adds one.
fn print_progress(event: &Event) {
    if let EventKind::MessageEnd {
        message: Message::Assistant(message),
        ..
    } = &event.event
    {
        if let Some((_, line)) = answer_end_line(message) {
            eprint_line(&line);
        }
    } else if let Some((_, line)) = progress_line(&event.event) {
        eprint_line(&line);
    }
}
