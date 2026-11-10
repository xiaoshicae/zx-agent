//! `zx-agent run`: one task to completion without a person; the exit code reports the outcome.

use std::io::{IsTerminal, Read};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;

use zx_agent::AgentSession;
use zx_agent::agent::{AgentEvent, Event, EventSink};
use zx_agent::ai::Message;
use zx_agent::config::Builtin;
use zx_agent::runner::{RunOutcome, RunReport};

use crate::cli::{self, OutputMode, RunArgs};
use crate::process::{EXIT_DENIED, JsonlEvents, exit_code, print_line, run_sink, spawn_signal_handler, with_telemetry};
use crate::render::{answer_end_line, progress_line, visible};
use crate::session_files::SessionFile;

/// Run one task and return its exit code. An `Err` is a configuration problem found
/// before the run started.
pub(crate) async fn run(args: RunArgs) -> Result<u8> {
    let RunArgs {
        prompt,
        fail_on_denied,
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
            let session = base.session.as_deref().context("--continue needs --session")?;
            anyhow::ensure!(
                session.exists(),
                "--continue needs an existing session file; {} does not exist",
                session.display()
            );
            None
        }
    };
    with_telemetry(async move |telemetry| {
        let shared = cli::Shared::connect(base, budget, Builtin::Unattended)
            .await?
            .with_traces(telemetry.map(run_sink));
        let session = build_session(&shared, output, deltas).await?;
        let cancel = CancellationToken::new();
        spawn_signal_handler(cancel.clone());
        let report = match prompt {
            Some(prompt) => session.prompt(prompt, &cancel).await?,
            None => session.continue_run(&cancel).await?,
        };
        print_report(&report, output);
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

/// Assemble an unattended session of the `unattended` agent, or the user's file that
/// replaces it, without exposing interactive choices to the kernel.
async fn build_session(shared: &cli::Shared, output: OutputMode, deltas: bool) -> Result<AgentSession> {
    let file = SessionFile::from_path(shared.base.session.as_deref());
    let prepared = cli::prepare(shared, &shared.settings.model, None, file).await?;
    let sink: Arc<dyn EventSink> = match output {
        OutputMode::Text => Arc::new(TextProgress),
        OutputMode::Jsonl => Arc::new(JsonlEvents { deltas }),
    };
    prepared.into_session(sink)
}

fn print_report(report: &RunReport, output: OutputMode) {
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
            let session = report
                .session_file
                .as_deref()
                .map(|file| format!(" session={file}"))
                .unwrap_or_default();
            eprintln!(
                "[zx-agent] outcome={:?} model_requests={} retries={} tokens={} (in {} / out {}){cost}{denied}{error}{session}",
                report.outcome, report.model_requests, report.retries, usage.total_tokens, usage.input, usage.output,
            );
        }
        // The `run_end` event, already written, carries the report.
        OutputMode::Jsonl => {}
    }
}

/// Short progress lines on stderr, printed like chat's; the answer is printed with the report.
struct TextProgress;

impl EventSink for TextProgress {
    fn emit(&self, event: &Event) {
        if let AgentEvent::MessageEnd {
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
}
