//! `zx-agent run`: one task to completion without a person; the exit code reports the outcome.

use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use tokio_util::sync::CancellationToken;
use zx_agent::ai::{Image, Message};
use zx_agent::builtin::images;
use zx_agent::builtin::tools::DEFAULT_MAX_FILE_BYTES;
use zx_agent::events::{Event, EventKind, RunOutcome, RunReport};
use zx_agent::input::Input;
use zx_agent::{Run, Session};
use zx_app::config::Builtin;
use zx_app::otel::{Telemetry, TraceHook, TraceParent};
use zx_app::shutdown::next_event;
use zx_app::{Application, Approvals, SessionPlace};
use zx_util::stdio::{eprint_line, print_line};
use zx_util::text::amount;

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
        approvals,
        output,
        deltas,
    } = args;
    let task = Task::of(prompt, continue_session, session).await?;
    with_telemetry(async move |telemetry| {
        let app = connect(cli::overrides(base, &budget, &approvals))
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
    /// A prompt and its images, in `--session`'s file when one is given, else in memory.
    Prompt { prompt: Input, file: Option<PathBuf> },
    /// `--continue`: what the conversation in `--session`'s file ends with.
    Continue { file: PathBuf },
}

impl Task {
    /// The task that the prompt's `arguments`, `--continue` and `--session`'s `file`
    /// describe: the prompt read, and the images its `@` arguments name, fitted to the model
    /// APIs' limits. The parser takes a prompt or `--continue`, not both, and `--continue`
    /// only with `--session`; the last error is for what it refuses.
    async fn of(arguments: Vec<String>, continue_session: bool, file: Option<PathBuf>) -> Result<Self> {
        match (arguments.is_empty(), continue_session, file) {
            (false, false, file) => {
                let (paths, text) = prompt_arguments(arguments)?;
                let text = match text {
                    Some(text) => read_prompt(&text)?,
                    None => String::new(),
                };
                let images = attached_images(&paths).await?;
                // The kernel takes any input; an empty one would reach the model as a message.
                anyhow::ensure!(!text.trim().is_empty() || !images.is_empty(), "the prompt is empty");
                let prompt = Input::User {
                    text,
                    images,
                    actor: None,
                };
                Ok(Self::Prompt { prompt, file })
            }
            (true, true, Some(file)) => {
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

/// The prompt's arguments apart: the paths of those that start with `@`, the images to
/// attach (pi: `@files`), and the one argument left, the description, if any.
fn prompt_arguments(arguments: Vec<String>) -> Result<(Vec<PathBuf>, Option<String>)> {
    let (attached, mut descriptions): (Vec<String>, Vec<String>) =
        arguments.into_iter().partition(|argument| argument.starts_with('@'));
    let paths = attached
        .iter()
        .map(|argument| match &argument[1..] {
            "" => Err(anyhow!("an @ argument names no file: write @<path>")),
            path => Ok(PathBuf::from(path)),
        })
        .collect::<Result<Vec<_>>>()?;
    anyhow::ensure!(
        descriptions.len() <= 1,
        "give one task description, not {}: quote a description that has spaces",
        descriptions.len()
    );
    Ok((paths, descriptions.pop()))
}

/// The description an argument gives: itself, or with "-" what stdin holds.
fn read_prompt(arg: &str) -> Result<String> {
    if arg != "-" {
        return Ok(arg.to_string());
    }
    // A startup read: the session and the runtime's other tasks do not exist yet, so nothing
    // waits while stdin is read to its end.
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .context("cannot read the prompt from stdin")?;
    Ok(text)
}

/// The images that `paths` name, each fitted to the model APIs' limits; an error names the
/// path it could not attach and says why.
async fn attached_images(paths: &[PathBuf]) -> Result<Vec<Image>> {
    let mut attached = Vec::with_capacity(paths.len());
    for path in paths {
        let cannot_attach = || format!("cannot attach @{}", path.display());
        let bytes = read_image_file(path).with_context(cannot_attach)?;
        let fitted = images::fit(bytes)
            .await
            .map_err(|reason| anyhow!("{}: {reason}", cannot_attach()))?;
        attached.push(fitted.image);
    }
    Ok(attached)
}

/// The bytes of the regular file at `path`, at most as many as read_file reads: a startup read,
/// like the prompt's from stdin. A relative path is the launch directory's, as a shell has it.
fn read_image_file(path: &Path) -> Result<Vec<u8>> {
    let found = std::fs::metadata(path)?;
    anyhow::ensure!(found.is_file(), "it is not a regular file");
    anyhow::ensure!(
        found.len() <= DEFAULT_MAX_FILE_BYTES,
        "it is larger than {DEFAULT_MAX_FILE_BYTES} bytes"
    );
    Ok(std::fs::read(path)?)
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
    let agent = app.build(Builtin::Unattended.name(), app.model(), &place, Approvals::Nobody)?;
    // Before the run, as the application's notices are when it connects.
    for notice in agent.notices() {
        eprint_line(&visible(notice));
    }
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
            if answered(report.outcome) && !report.final_text.is_empty() {
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
                .map(|cost| format!(" cost={}", amount(cost)))
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

/// Whether the run ended with an answer for stdout: a complete one, or one cut off at the
/// output token limit, which holds what was produced. The text a failed, timed-out or
/// cancelled run, or one whose budget ran out before the model answered, left behind is no
/// answer; it stays in the session and in jsonl's `run_end` (pi: `runPrintMode`).
fn answered(outcome: RunOutcome) -> bool {
    match outcome {
        RunOutcome::Completed | RunOutcome::Truncated => true,
        RunOutcome::BudgetExhausted { truncated, .. } => truncated,
        RunOutcome::Failed | RunOutcome::TimedOut | RunOutcome::Cancelled => false,
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
