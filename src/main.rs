use std::io::{IsTerminal, Read, Write};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

use zx_agent::AgentSession;
use zx_agent::agent::{AgentEvent, Event, EventSink};
use zx_agent::ai::{Message, StopReason};
use zx_agent::profile::Builtin;
use zx_agent::runner::{RunOutcome, RunReport};

mod chat;
mod cli;
mod serve;

use chat::visible;
use cli::{Cli, Command, OutputMode, RunArgs};

/// Exit codes, so schedulers can react without parsing output.
const EXIT_COMPLETED: u8 = 0;
const EXIT_ERROR: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_MAX_TURNS: u8 = 3;
const EXIT_TIMEOUT: u8 = 4;
const EXIT_ABORTED: u8 = 5;
const EXIT_TRUNCATED: u8 = 6;

#[tokio::main]
async fn main() -> ExitCode {
    #[cfg(target_os = "linux")]
    if let Err(error) = deny_same_user_inspection() {
        eprintln!("[security] cannot stop commands from reading this process's environment: {error}");
    }
    let result = match Cli::parse().command {
        Command::Run(args) => run(args).await,
        Command::Chat(args) => chat::run(args).await,
        Command::Serve(args) => serve::run(args).await,
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("zx-agent: {e:#}");
            ExitCode::from(EXIT_USAGE)
        }
    }
}

/// Mark this process non-dumpable before any command runs. Commands run as the same user,
/// and ptrace access mode checks then refuse them this process's `/proc/<pid>/environ`,
/// which holds the API key, unless they have CAP_SYS_PTRACE (ptrace(2),
/// proc_pid_environ(5)). The same attribute turns off core dumps of this process
/// (PR_SET_DUMPABLE(2const)). See docs/p6-online-access-review.md, section 3.3.
#[cfg(target_os = "linux")]
fn deny_same_user_inspection() -> std::io::Result<()> {
    // prctl(2): int prctl(int op, ... /* unsigned long arg2, arg3, arg4, arg5 */).
    const SUID_DUMP_DISABLE: libc::c_ulong = 0;
    const UNUSED: libc::c_ulong = 0;
    // SAFETY: PR_SET_DUMPABLE reads only its integer arguments, passed with the type the
    // call expects; no pointer into this process is handed to the kernel.
    let status = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, SUID_DUMP_DISABLE, UNUSED, UNUSED, UNUSED) };
    if status == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Run one task and return its exit code. An `Err` is a configuration problem found
/// before the run started.
async fn run(args: RunArgs) -> Result<u8> {
    let prompt = read_prompt(&args.prompt)?;
    let session = build_session(&args).await?;
    let cancel = CancellationToken::new();
    spawn_signal_handler(cancel.clone());
    let report = session.prompt_with_cancel(prompt, &cancel).await?;
    print_report(&report, args.output);
    Ok(exit_code(report.outcome))
}

fn read_prompt(arg: &str) -> Result<String> {
    let prompt = if arg == "-" {
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

/// Assemble the unattended preset without exposing interactive choices to the kernel.
async fn build_session(args: &RunArgs) -> Result<AgentSession> {
    let instructions = cli::read_instructions(&args.base)?;
    let prepared = cli::prepare(cli::SessionOptions {
        base: &args.base,
        transport: &args.transport,
        request: &args.request,
        model: &args.base.model,
        file: cli::SessionFile::from_path(args.request.session.as_deref()),
        default_profile: Builtin::Troubleshoot,
        budget: cli::Budget {
            max_turns: args.max_turns,
            timeout_secs: args.timeout_secs,
            continue_on_length: args.continue_on_length,
        },
        instructions: instructions.as_deref(),
        hidden_env: &[],
    })
    .await?;
    let sink: Arc<dyn EventSink> = match args.output {
        OutputMode::Text => Arc::new(TextProgress),
        OutputMode::Jsonl => Arc::new(JsonlEvents { deltas: args.deltas }),
    };
    prepared.into_session(sink)
}

fn print_report(report: &RunReport, output: OutputMode) {
    match output {
        OutputMode::Text => {
            if !report.final_text.is_empty() {
                // A terminal would run control sequences in the answer; a pipe gets the exact text.
                if std::io::stdout().is_terminal() {
                    println!("{}", visible(&report.final_text));
                } else {
                    println!("{}", report.final_text);
                }
            }
            let usage = &report.usage;
            let error = report
                .error
                .as_deref()
                .map(|e| format!(" error={}", visible(e)))
                .unwrap_or_default();
            let session = report
                .session_file
                .as_deref()
                .map(|file| format!(" session={file}"))
                .unwrap_or_default();
            eprintln!(
                "[zx-agent] outcome={:?} turns={} retries={} tokens={} (in {} / out {}){error}{session}",
                report.outcome, report.turns, report.retries, usage.total_tokens, usage.input, usage.output,
            );
        }
        // The `run_end` event, already written, carries the report.
        OutputMode::Jsonl => {}
    }
}

fn exit_code(outcome: RunOutcome) -> u8 {
    match outcome {
        RunOutcome::Completed => EXIT_COMPLETED,
        RunOutcome::Error => EXIT_ERROR,
        RunOutcome::MaxTurns => EXIT_MAX_TURNS,
        RunOutcome::Timeout => EXIT_TIMEOUT,
        RunOutcome::Aborted => EXIT_ABORTED,
        RunOutcome::Truncated => EXIT_TRUNCATED,
    }
}

/// SIGINT and SIGTERM cancel the run; the report is still written.
///
/// The listeners are registered before this returns, so a signal that arrives once the run
/// has started always cancels it instead of killing the process. The waiting task is detached
/// on purpose: it lives until a signal arrives or the process exits.
fn spawn_signal_handler(cancel: CancellationToken) {
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
fn write_line(value: &impl serde::Serialize) {
    // Events and reports hold only strings, numbers, and `serde_json::Value`, which always serialize.
    let Ok(line) = serde_json::to_string(value) else {
        return;
    };
    let mut out = std::io::stdout().lock();
    // A closed stdout (for example a consumer that exited) leaves nobody to report to; the run
    // continues and the exit code still tells the scheduler how it ended.
    let _ = writeln!(out, "{line}").and_then(|()| out.flush());
}

/// Writes every event as one JSON line on stdout.
struct JsonlEvents {
    deltas: bool,
}

impl EventSink for JsonlEvents {
    fn emit(&self, event: &Event) {
        if !self.deltas && matches!(event.event, AgentEvent::MessageUpdate { .. }) {
            return;
        }
        write_line(event);
    }
}

/// Short progress lines on stderr. Tool names, arguments and errors come from the model or
/// the server, so they are escaped like chat output.
struct TextProgress;

impl EventSink for TextProgress {
    fn emit(&self, event: &Event) {
        match &event.event {
            AgentEvent::ToolExecutionStart { tool_name, args, .. } => {
                eprintln!(
                    "[tool] {} {}",
                    visible(tool_name),
                    visible(&clip(&args.to_string(), 300))
                );
            }
            AgentEvent::ToolExecutionEnd {
                tool_name,
                is_error,
                duration_ms,
                ..
            } => {
                let status = if *is_error { "error" } else { "ok" };
                let duration = duration_ms.map(|ms| format!(" {ms}ms")).unwrap_or_default();
                eprintln!("[tool] {} -> {status}{duration}", visible(tool_name));
            }
            AgentEvent::MessageEnd {
                message: Message::Assistant(message),
                ..
            } => {
                if let Some(error) = &message.error_message {
                    eprintln!("[model] {:?}: {}", message.stop_reason, visible(error));
                } else if message.stop_reason == StopReason::Length {
                    eprintln!("[model] Length: the response hit the output token limit");
                }
            }
            AgentEvent::RetryScheduled {
                attempt,
                max_retries,
                delay_ms,
                error,
            } => {
                eprintln!("[retry] {attempt}/{max_retries} in {delay_ms}ms: {}", visible(error));
            }
            AgentEvent::CompactionStart { input_bytes, .. } => {
                eprintln!("[compact] summarizing {input_bytes} context bytes")
            }
            AgentEvent::CompactionEnd {
                input_bytes,
                output_bytes,
                ..
            } => eprintln!("[compact] {input_bytes} -> {output_bytes} context bytes"),
            AgentEvent::CompactionFailed { error, .. } => eprintln!("[compact] {}", visible(error)),
            AgentEvent::ToolCallRecovered {
                tool_call_id,
                tool_name,
                state,
            } => eprintln!(
                "[recover] {} {}: {}",
                visible(tool_name),
                visible(tool_call_id),
                state.describe()
            ),
            AgentEvent::RunStart { .. }
            | AgentEvent::RunEnd { .. }
            | AgentEvent::AgentStart { .. }
            | AgentEvent::TurnStart { .. }
            | AgentEvent::RequestStart { .. }
            | AgentEvent::MessageStart { .. }
            | AgentEvent::MessageUpdate { .. }
            | AgentEvent::MessageEnd { .. }
            | AgentEvent::ToolDecision { .. }
            | AgentEvent::ApprovalRequested { .. }
            | AgentEvent::ApprovalResolved { .. }
            | AgentEvent::TurnEnd { .. }
            | AgentEvent::AgentEnd { .. }
            | AgentEvent::ContextEdited { .. }
            | AgentEvent::HookFailed { .. } => {}
        }
    }
}

fn clip(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((index, _)) => format!("{}...", &text[..index]),
        None => text.to_string(),
    }
}
