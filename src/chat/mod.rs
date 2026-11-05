//! The line-oriented terminal frontend; model and tool work stays in AgentSession.

mod commands;
mod debug;
mod input;
mod interaction;
mod output;
mod signals;

use std::ffi::OsStr;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::mpsc;

use zx_agent::AgentSession;
use zx_agent::agent::EventSink;
use zx_agent::agent_session::{ModelRef, Subscription};
use zx_agent::ai::{Provider, Reasoning};
use zx_agent::compaction;
use zx_agent::otel::Telemetry;
use zx_agent::profile::Builtin;
use zx_agent::runner::RunReport;

use crate::cli::{self, ChatArgs, SessionFile, SessionOptions};
use crate::{EXIT_ABORTED, EXIT_COMPLETED};

use commands::Command;
use input::Line;
use interaction::{TerminalInteraction, Ticket};
use output::Output;
pub(crate) use output::{progress_line, visible};
use signals::{Interrupt, Signals};

#[derive(Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Terminal,
    Script,
}

enum Flow {
    Continue,
    Prompt(String),
    New,
    Exit(u8),
}

struct Built {
    session: AgentSession,
    provider: Arc<dyn Provider>,
    cwd: PathBuf,
    /// The trace export of this session, while it lasts.
    _traced: Option<Subscription>,
}

struct Settings {
    args: ChatArgs,
    /// `--system-prompt-file`, read once: it may be a pipe, and /new reuses the text.
    instructions: Option<String>,
    /// Connected once; /new reuses the connections.
    mcp: cli::McpTools,
    /// Where traces go, when an OTLP endpoint is configured.
    traces: Option<Arc<dyn EventSink>>,
}

async fn build(
    settings: &Settings,
    model: &str,
    file: SessionFile<'_>,
    interaction: &Arc<TerminalInteraction>,
    output: &Arc<Output>,
) -> Result<Built> {
    let args = &settings.args;
    let mut prepared = cli::prepare(SessionOptions {
        base: &args.base,
        transport: &args.transport,
        request: &args.request,
        model,
        file,
        default_profile: Builtin::Assistant,
        budget: cli::Budget {
            max_model_requests: args.max_model_requests,
            timeout_secs: args.timeout_secs,
            token_budget: args.token_budget,
            continue_on_length: None,
        },
        instructions: settings.instructions.as_deref(),
        hidden_env: &[],
        mcp: &settings.mcp,
    })
    .await?;
    prepared.require_bash_approval("shell commands require interactive approval");
    prepared.config.interaction = interaction.clone();
    prepared.config.interaction_timeout = std::time::Duration::from_secs(settings.args.approval_timeout_secs);
    let provider = prepared.config.spec.provider.clone();
    let cwd = prepared.config.spec.cwd.clone();
    let session = prepared.into_session(output.clone())?;
    let traced = settings.traces.clone().map(|sink| session.subscribe(sink));
    Ok(Built {
        session,
        provider,
        cwd,
        _traced: traced,
    })
}

pub(crate) async fn run(args: ChatArgs) -> Result<u8> {
    let telemetry = cli::start_telemetry()?;
    let traces = telemetry.as_ref().map(Telemetry::sink);
    let result = converse(args, traces).await;
    cli::finish_telemetry(telemetry).await;
    result
}

async fn converse(args: ChatArgs, traces: Option<Arc<dyn EventSink>>) -> Result<u8> {
    let instructions = cli::read_instructions(&args.base)?;
    let mcp = cli::connect_mcp(&args.base, Builtin::Assistant, &args.transport).await?;
    let settings = Settings {
        args,
        instructions,
        mcp,
        traces,
    };
    let args = &settings.args;
    let mode = if std::io::stdin().is_terminal() {
        InputMode::Terminal
    } else {
        InputMode::Script
    };
    let (interaction, mut approvals) = TerminalInteraction::channel();
    let interaction = Arc::new(interaction);
    let output = Arc::new(Output::new(args.output, args.deltas, args.debug));
    let mut model = args.base.model.clone();
    let mut reasoning = args.request.reasoning;
    let mut built = build(
        &settings,
        &model,
        SessionFile::from_path(args.request.session.as_deref()),
        &interaction,
        &output,
    )
    .await?;
    let mut signals = Signals::new().context("cannot register chat signal handlers")?;
    let mut input = input::spawn()?;
    eprintln!("[chat] model={}; /help for commands", visible(&model));
    let mut next_prompt = None;

    loop {
        if mode == InputMode::Terminal {
            prompt();
        }
        let line = if let Some(text) = next_prompt.take() {
            Line::Text(text)
        } else {
            tokio::select! {
                biased;
                interrupt = signals.next() => {
                    match interrupt {
                        Interrupt::Cancel => { eprintln!("\n[chat] no active task; /quit to exit"); continue; }
                        Interrupt::Terminate => return Ok(EXIT_ABORTED),
                    }
                }
                line = input.recv() => match line {
                    Some(line) => line.context("cannot read terminal input")?,
                    None => return Ok(EXIT_COMPLETED),
                },
            }
        };
        let mut io = TurnIo {
            input: &mut input,
            approvals: &mut approvals,
            signals: &mut signals,
            mode,
            output: &output,
            cwd: &built.cwd,
        };
        let line = match line {
            Line::Text(text) => text,
            Line::Rejected(reason) => {
                rejected(&reason);
                continue;
            }
        };
        let action = match commands::parse(line) {
            Command::Prompt(text) => drive_turn(&built.session, Operation::Prompt(text), &mut io).await?,
            Command::Compact => drive_turn(&built.session, Operation::Compact, &mut io).await?,
            Command::Continue => drive_turn(&built.session, Operation::Continue, &mut io).await?,
            Command::New => Flow::New,
            Command::Model(None) => {
                eprintln!("[chat] model={}", visible(&model));
                Flow::Continue
            }
            Command::Reasoning(None) => {
                eprintln!("[chat] reasoning={}", reasoning_name(reasoning));
                Flow::Continue
            }
            Command::Reasoning(Some(next)) => match parse_level(&next) {
                Ok(level) => {
                    built.session.set_reasoning(level)?;
                    reasoning = level;
                    eprintln!("[chat] reasoning={}", reasoning_name(reasoning));
                    Flow::Continue
                }
                Err(message) => {
                    eprintln!("[chat] {}", visible(&message));
                    Flow::Continue
                }
            },
            Command::Model(Some(next)) => {
                built.session.set_model(ModelRef {
                    provider: built.provider.clone(),
                    model: next.clone(),
                })?;
                model = next;
                eprintln!("[chat] model={}", visible(&model));
                Flow::Continue
            }
            Command::Quit => Flow::Exit(EXIT_COMPLETED),
            Command::Help => {
                help();
                Flow::Continue
            }
            Command::Invalid(message) => {
                eprintln!("[chat] {}", visible(&message));
                Flow::Continue
            }
            Command::Empty => Flow::Continue,
        };
        match action {
            Flow::Continue => {}
            Flow::Prompt(text) => next_prompt = Some(text),
            Flow::Exit(code) => return Ok(code),
            Flow::New => {
                let path = new_file(args.request.session.as_deref());
                let file = path.as_deref().map_or(SessionFile::Memory, SessionFile::Create);
                match build(&settings, &model, file, &interaction, &output).await {
                    Ok(next) => {
                        built = next;
                        let file = path
                            .as_deref()
                            .map(|path| format!("; session={}", path.display()))
                            .unwrap_or_default();
                        eprintln!("[chat] new conversation{file}");
                    }
                    Err(error) => eprintln!("[chat] cannot create new conversation: {error:#}"),
                }
            }
        }
    }
}

struct TurnIo<'a> {
    input: &'a mut input::Input,
    approvals: &'a mut mpsc::Receiver<Ticket>,
    signals: &'a mut Signals,
    mode: InputMode,
    output: &'a Output,
    cwd: &'a Path,
}

enum Operation {
    Prompt(String),
    Continue,
    Compact,
}
enum Report {
    Task(RunReport),
    Compact(compaction::Report),
}

impl Report {
    fn print(&self, output: &Output) {
        match self {
            Self::Task(report) => output.report(report),
            Self::Compact(report) => output.compact_report(report),
        }
    }
}

async fn drive_turn(session: &AgentSession, operation: Operation, io: &mut TurnIo<'_>) -> Result<Flow> {
    let compact = matches!(operation, Operation::Compact);
    let running = async {
        match operation {
            Operation::Prompt(text) => session.prompt(text).await.map(Report::Task),
            Operation::Continue => session.continue_run().await.map(Report::Task),
            Operation::Compact => session.compact().await.map(Report::Compact),
        }
    };
    tokio::pin!(running);
    let mut pending: Option<Ticket> = None;
    let mut discussing = false;
    let mut queued_prompt = None;
    loop {
        if pending.as_ref().is_some_and(|ticket| ticket.reply.is_closed()) {
            pending = None;
            discussing = false;
        }
        tokio::select! {
            biased;
            interrupt = io.signals.next() => {
                session.abort();
                drop(pending.take());
                let report = running.await?;
                report.print(io.output);
                if queued_prompt.is_some() { eprintln!("[chat] queued input cleared by cancellation"); }
                // Steering meant for the cancelled task must not reach the next one.
                let dropped = unread_input(session);
                if !dropped.is_empty() {
                    eprintln!(
                        "[chat] cancellation discarded {} steering input(s): {}",
                        dropped.len(),
                        visible(&dropped.join(" / "))
                    );
                }
                return Ok(match interrupt {
                    Interrupt::Cancel => Flow::Continue,
                    Interrupt::Terminate => Flow::Exit(EXIT_ABORTED),
                });
            }
            report = &mut running => {
                report?.print(io.output);
                // Steering that arrived as the task ended was never read: ask it next.
                let late = unread_input(session);
                if !late.is_empty() {
                    eprintln!("[chat] steering input arrived as the task ended; sending it as the next prompt");
                }
                let next: Vec<String> = queued_prompt.into_iter().chain(late).collect();
                return Ok(if next.is_empty() { Flow::Continue } else { Flow::Prompt(next.join("\n\n")) });
            }
            () = expired(&mut pending), if pending.is_some() => {
                discussing = false;
                if let Some(ticket) = pending.take() {
                    eprintln!("[approval] request {} expired; the task goes on without the call", ticket.id);
                }
            }
            request = io.approvals.recv() => {
                let Some(ticket) = request else { anyhow::bail!("terminal approval channel closed"); };
                if ticket.reply.is_closed() { continue; }
                if let Some(previous) = pending.take() {
                    // Defensive fail-closed behavior; the core normally asks serially.
                    deny(previous, "superseded terminal approval");
                }
                discussing = false;
                show_approval(&ticket, io.cwd);
                pending = Some(ticket);
            }
            line = io.input.recv(), if io.mode == InputMode::Terminal || pending.is_some() => {
                let command = match line {
                    Some(line) => match line.context("cannot read terminal input")? {
                        Line::Text(text) => {
                            if pending.is_some() && text.trim() != "/quit" && text.trim() != "/new" {
                                if discussing {
                                    if text.trim().is_empty() {
                                        eprintln!("[approval] enter a message to discuss this call");
                                    } else if let Some(ticket) = pending.take() {
                                        session.steer(text);
                                        deny(ticket, "user wants to discuss this call");
                                        discussing = false;
                                    }
                                } else {
                                    match text.trim() {
                                        "1" | "2" => {
                                            let decision = if text.trim() == "1" { zx_agent::interaction::Approval::AllowOnce } else { zx_agent::interaction::Approval::AllowSession };
                                            if let Some(ticket) = pending.take()
                                                && ticket.reply.send(decision).is_err() {
                                                eprintln!("[approval] request expired");
                                            }
                                        }
                                        "3" => { if let Some(ticket) = pending.take() { deny(ticket, "declined by the user"); } }
                                        "4" => { discussing = true; eprintln!("[approval] What would you like to discuss? The call will not run."); }
                                        _ => eprintln!("[approval] choose 1, 2, 3 or 4"),
                                    }
                                }
                                continue;
                            }
                            commands::parse(text)
                        },
                        Line::Rejected(reason) => {
                            rejected(&reason);
                            continue;
                        }
                    },
                    None => {
                        if let Some(ticket) = pending.take() { deny(ticket, "terminal input closed"); }
                        session.abort();
                        running.await?.print(io.output);
                        return Ok(Flow::Exit(EXIT_COMPLETED));
                    }
                };
                match command {
                    Command::Quit | Command::New => {
                        let next = if matches!(command, Command::New) { Flow::New } else { Flow::Exit(EXIT_COMPLETED) };
                        session.abort();
                        drop(pending.take());
                        running.await?.print(io.output);
                        return Ok(next);
                    }
                    Command::Prompt(text) if compact => {
                        if queued_prompt.is_none() {
                            queued_prompt = Some(text);
                            eprintln!("[chat] input queued after compaction");
                        } else { eprintln!("[chat] one input is already queued; wait for compaction or cancel it"); }
                    }
                    Command::Prompt(text) if pending.is_none() => {
                        session.steer(text);
                        eprintln!("[chat] steering input queued");
                    }
                    Command::Prompt(_) => unreachable!("pending approval input is handled above"),
                    Command::Model(_) => eprintln!("[chat] wait for the current task or cancel it before changing models"),
                    Command::Reasoning(_) => eprintln!("[chat] wait for the current task or cancel it before changing the reasoning level"),
                    Command::Help => help(),
                    Command::Compact => eprintln!("[chat] session is busy; wait for the current operation or cancel it before /compact"),
                    Command::Continue => eprintln!("[chat] session is busy; wait for the current operation or cancel it before /continue"),
                    Command::Invalid(message) => eprintln!("[chat] {}", visible(&message)),
                    Command::Empty => {}
                }
            }
        }
    }
}

/// Completes once the core stops waiting for the pending approval: it timed out, or the
/// task was cancelled. Never completes without one.
async fn expired(pending: &mut Option<Ticket>) {
    match pending {
        Some(ticket) => ticket.reply.closed().await,
        None => std::future::pending().await,
    }
}

/// Input that `steer` queued and no run read; a task only reads it between turns.
fn unread_input(session: &AgentSession) -> Vec<String> {
    session.take_queued().unwrap_or_else(|error| {
        eprintln!("[chat] cannot take the unread input: {error}");
        Vec::new()
    })
}

fn show_approval(ticket: &Ticket, cwd: &Path) {
    eprintln!("\n[approval {}] tool={}", ticket.id, visible(&ticket.request.call.name));
    if ticket.request.call.name == "bash"
        && let Some(command) = ticket
            .request
            .call
            .arguments
            .get("command")
            .and_then(|value| value.as_str())
    {
        eprintln!("[command] {}", visible(command));
        if let Some(arguments) = ticket.request.call.arguments.as_object() {
            let other: serde_json::Map<String, serde_json::Value> = arguments
                .iter()
                .filter(|(key, _)| key.as_str() != "command")
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            if !other.is_empty() {
                eprintln!("[arguments] {}", visible(&serde_json::Value::Object(other).to_string()));
            }
        }
    } else {
        eprintln!("[arguments] {}", visible(&ticket.request.call.arguments.to_string()));
    }
    eprintln!("[reason] {}", visible(&ticket.request.reason));
    eprintln!("[scope] runs in {:?} with this process's permissions", cwd);
    eprintln!(
        "[approval] 1. Allow once\n           2. Allow identical calls in this conversation\n           3. Deny\n           4. Chat about this\n[approval] Choose 1, 2, 3 or 4:"
    );
}

/// A discarded input line ends neither the conversation nor the current task.
fn rejected(reason: &str) {
    eprintln!("[chat] input rejected: {}", visible(reason));
}

fn deny(ticket: Ticket, reason: &str) {
    if ticket
        .reply
        .send(zx_agent::interaction::Approval::Deny(reason.into()))
        .is_err()
    {
        // The runtime already stopped waiting; no call is authorized.
        eprintln!("[approval] request {} expired", ticket.id);
    }
}

fn new_file(original: Option<&Path>) -> Option<PathBuf> {
    original.map(|original| {
        let mut name = original
            .file_stem()
            .unwrap_or_else(|| OsStr::new("session"))
            .to_os_string();
        name.push(format!(".{}.jsonl", uuid::Uuid::new_v4()));
        original.with_file_name(name)
    })
}

fn prompt() {
    let mut stderr = std::io::stderr().lock();
    // A closed terminal has nobody to receive another prompt.
    let _ = stderr.write_all(b"zx> ").and_then(|()| stderr.flush());
}

/// A level for /reasoning, or `None` for `default`.
fn parse_level(text: &str) -> Result<Option<Reasoning>, String> {
    if text.trim().eq_ignore_ascii_case("default") {
        return Ok(None);
    }
    text.parse().map(Some)
}

fn reasoning_name(reasoning: Option<Reasoning>) -> &'static str {
    reasoning.map_or("default", Reasoning::as_str)
}

fn help() {
    eprintln!(
        "/new                  start a fresh conversation\n\
               /model [id]           show or change the model while idle\n\
               /reasoning [level]    show or change the reasoning level while idle: off, minimal,\n\
                                     low, medium, high, xhigh, or default to leave it to the server\n\
               /compact              summarize old context while retaining raw history\n\
               /continue             continue without new input, such as after a failed answer\n\
               /quit                 exit chat\n\
               Ctrl+C                cancel the current task; keep the conversation"
    );
}
