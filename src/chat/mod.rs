//! The line-oriented terminal frontend; model and tool work stays in AgentSession.

mod commands;
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
use zx_agent::agent::Decision;
use zx_agent::agent_session::ModelRef;
use zx_agent::ai::Provider;
use zx_agent::extensions::ContinueOnLength;
use zx_agent::policy::{PolicyHook, Rule, RuleMatcher, RulePolicy};
use zx_agent::profile::Builtin;

use crate::cli::{self, ChatArgs, Explicit, SessionFile, SessionOptions};
use crate::{EXIT_ABORTED, EXIT_COMPLETED};

use commands::Command;
use interaction::{TerminalInteraction, Ticket};
use output::{Output, visible};
use signals::{Interrupt, Signals};

#[derive(Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Terminal,
    Script,
}

enum Flow {
    Continue,
    New,
    Exit(u8),
}

struct Built {
    session: AgentSession,
    provider: Arc<dyn Provider>,
    cwd: PathBuf,
}

struct Settings {
    args: ChatArgs,
    explicit: Explicit,
}

fn build(
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
            max_turns: args.max_turns,
            timeout_secs: args.timeout_secs,
            continue_on_length: 0,
        },
        explicit: settings.explicit,
    })?;
    prepared
        .config
        .hooks
        .push(PolicyHook::new(Arc::new(RulePolicy::new(vec![
            Rule {
                matcher: RuleMatcher::Tool("bash".into()),
                decision: Decision::Ask("shell commands require interactive approval".into()),
            },
            Rule {
                matcher: RuleMatcher::Any,
                decision: Decision::Allow,
            },
        ]))));
    prepared
        .config
        .hooks
        .push(ContinueOnLength::new(prepared.continue_on_length));
    prepared.config.hooks.push(prepared.redactor);
    prepared.config.interaction = interaction.clone();
    let provider = prepared.config.spec.provider.clone();
    let cwd = prepared.config.spec.cwd.clone();
    let session = AgentSession::new(
        prepared.config,
        prepared.retry,
        prepared.limits,
        prepared.store,
        output.clone(),
    );
    Ok(Built { session, provider, cwd })
}

pub(crate) async fn run(args: ChatArgs, explicit: Explicit) -> Result<u8> {
    let settings = Settings { args, explicit };
    let args = &settings.args;
    let mode = if std::io::stdin().is_terminal() {
        InputMode::Terminal
    } else {
        InputMode::Script
    };
    let (interaction, mut approvals) = TerminalInteraction::channel();
    let interaction = Arc::new(interaction);
    let output = Arc::new(Output::new(args.output, args.deltas));
    let mut model = args.base.model.clone();
    let mut built = build(
        &settings,
        &model,
        SessionFile::from_path(args.request.session.as_deref()),
        &interaction,
        &output,
    )?;
    let mut signals = Signals::new().context("cannot register chat signal handlers")?;
    let mut input = input::spawn()?;
    eprintln!("[chat] model={}; /help for commands", visible(&model));

    loop {
        if mode == InputMode::Terminal {
            prompt();
        }
        let line = tokio::select! {
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
        };
        let action = match commands::parse(line) {
            Command::Prompt(text) => {
                let mut io = TurnIo {
                    input: &mut input,
                    approvals: &mut approvals,
                    signals: &mut signals,
                    mode,
                    output: &output,
                    cwd: &built.cwd,
                };
                drive_turn(&built.session, text, &mut io).await?
            }
            Command::New => Flow::New,
            Command::Model(None) => {
                eprintln!("[chat] model={}", visible(&model));
                Flow::Continue
            }
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
            Command::Compact => {
                compact_unavailable();
                Flow::Continue
            }
            Command::Approval { .. } => {
                eprintln!("[approval] no pending request");
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
            Flow::Exit(code) => return Ok(code),
            Flow::New => {
                let path = new_file(args.request.session.as_deref());
                let file = path.as_deref().map_or(SessionFile::Memory, SessionFile::Create);
                match build(&settings, &model, file, &interaction, &output) {
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

async fn drive_turn(session: &AgentSession, text: String, io: &mut TurnIo<'_>) -> Result<Flow> {
    let running = session.prompt(text);
    tokio::pin!(running);
    let mut pending: Option<Ticket> = None;
    loop {
        if pending.as_ref().is_some_and(|ticket| ticket.reply.is_closed()) {
            pending = None;
        }
        tokio::select! {
            biased;
            interrupt = io.signals.next() => {
                session.abort();
                drop(pending.take());
                let report = running.await?;
                io.output.report(&report);
                return Ok(match interrupt {
                    Interrupt::Cancel => Flow::Continue,
                    Interrupt::Terminate => Flow::Exit(EXIT_ABORTED),
                });
            }
            report = &mut running => {
                io.output.report(&report?);
                return Ok(Flow::Continue);
            }
            request = io.approvals.recv() => {
                let Some(ticket) = request else { anyhow::bail!("terminal approval channel closed"); };
                if ticket.reply.is_closed() { continue; }
                if let Some(previous) = pending.take() {
                    // Defensive fail-closed behavior; the core normally asks serially.
                    deny(previous, "superseded terminal approval");
                }
                show_approval(&ticket, io.cwd);
                pending = Some(ticket);
            }
            line = io.input.recv(), if io.mode == InputMode::Terminal || pending.is_some() => {
                let command = match line {
                    Some(line) => commands::parse(line.context("cannot read terminal input")?),
                    None => {
                        if let Some(ticket) = pending.take() { deny(ticket, "terminal input closed"); }
                        session.abort();
                        io.output.report(&running.await?);
                        return Ok(Flow::Exit(EXIT_COMPLETED));
                    }
                };
                match command {
                    Command::Approval { id, decision } => {
                        match pending.take() {
                            Some(ticket) if ticket.id == id => {
                                if ticket.reply.send(decision).is_err() {
                                    // A cancelled or timed-out confirmation cannot authorize a call.
                                    eprintln!("[approval] request {id} expired");
                                }
                            }
                            Some(ticket) => {
                                eprintln!("[approval] request {id} is not pending; answer request {}", ticket.id);
                                pending = Some(ticket);
                            }
                            None => eprintln!("[approval] no pending request"),
                        }
                    }
                    Command::Quit | Command::New => {
                        let next = if matches!(command, Command::New) { Flow::New } else { Flow::Exit(EXIT_COMPLETED) };
                        session.abort();
                        drop(pending.take());
                        io.output.report(&running.await?);
                        return Ok(next);
                    }
                    Command::Prompt(text) if pending.is_none() => {
                        session.steer(text);
                        eprintln!("[chat] steering input queued");
                    }
                    Command::Prompt(_) => eprintln!("[approval] use /approve <id> once|session or /deny <id> [reason]"),
                    Command::Model(_) => eprintln!("[chat] wait for the current task or cancel it before changing models"),
                    Command::Help => help(),
                    Command::Compact => compact_unavailable(),
                    Command::Invalid(message) => eprintln!("[chat] {}", visible(&message)),
                    Command::Empty => {}
                }
            }
        }
    }
}

fn show_approval(ticket: &Ticket, cwd: &Path) {
    eprintln!("\n[approval {}] tool={}", ticket.id, visible(&ticket.request.call.name));
    eprintln!("[arguments] {}", visible(&ticket.request.call.arguments.to_string()));
    eprintln!("[reason] {}", visible(&ticket.request.reason));
    eprintln!("[scope] runs in {:?} with this process's permissions", cwd);
    eprintln!(
        "[approval] /approve {} once | /approve {} session | /deny {} [reason]",
        ticket.id, ticket.id, ticket.id
    );
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

fn help() {
    eprintln!(
        "/new                  start a fresh conversation\n\
               /model [id]           show or change the model while idle\n\
               /compact              reserved for P5 context compaction\n\
               /approve <id> once    approve one exact call\n\
               /approve <id> session approve identical calls in this conversation\n\
               /deny <id> [reason]   reject the pending call\n\
               /quit                 exit chat\n\
               Ctrl+C                cancel the current task; keep the conversation"
    );
}

fn compact_unavailable() {
    eprintln!("[chat] /compact is not available yet; context compaction is planned for P5");
}
