//! The line-oriented terminal frontend; model and tool work stays in AgentSession.

mod commands;
mod debug;
mod input;
mod interaction;
mod output;
mod signals;
mod term;
mod tree;

use std::ffi::OsStr;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use zx_agent::AgentSession;
use zx_agent::agent::EventSink;
use zx_agent::agent_session::{ModelRef, Subscription};
use zx_agent::ai::{Message, Provider, Reasoning};
use zx_agent::compaction;
use zx_agent::interaction::Approval;
use zx_agent::otel::Telemetry;
use zx_agent::profile::Builtin;
use zx_agent::runner::RunReport;
use zx_agent::tools::BASH;

use crate::cli::{self, ChatArgs, SessionFile, SessionOptions};
use crate::sessions;
use crate::{EXIT_ABORTED, EXIT_COMPLETED};

use commands::Command;
use input::Line;
use interaction::{TerminalInteraction, Ticket};
use output::Output;
pub(crate) use output::{progress_line, visible};
use signals::{Interrupt, Signals};
use term::Tone;
use tree::Target;

/// How often the status line is redrawn while a task runs.
const TICK: Duration = Duration::from_secs(1);
/// A second Ctrl+C within this long, while idle, exits.
const QUIT_INTERRUPT_WINDOW: Duration = Duration::from_secs(2);
const PROMPT: &str = "zx> ";
const APPROVAL_MENU: &str = "  1/y allow once · 2 allow identical calls in this conversation · 3/n deny · 4 discuss";
const APPROVAL_PROMPT: &str = "[approval] Choose 1, 2, 3 or 4:";

#[derive(Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Terminal,
    Script,
}

enum Flow {
    Continue,
    Prompt(String),
    New,
    /// Switch to this session file.
    Resume(PathBuf),
    Exit(u8),
}

/// Where conversations are kept.
enum Storage {
    /// `--no-session`.
    Memory,
    /// `--session`: the file named; /new creates one beside it.
    File(PathBuf),
    /// A session directory: every new conversation gets its own file there.
    Directory(PathBuf),
}

impl Storage {
    /// The directory /sessions lists and /resume searches.
    fn dir(&self) -> Option<&Path> {
        match self {
            Self::Memory => None,
            Self::File(path) => Some(
                path.parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or(Path::new(".")),
            ),
            Self::Directory(dir) => Some(dir),
        }
    }
}

/// What a conversation runs on, shown at startup and by `/status`.
struct About {
    profile: String,
    tools: Vec<String>,
    auto_approve: Vec<String>,
    cwd: PathBuf,
    session: Option<PathBuf>,
    /// Set when the session file already held a conversation.
    resumed: Option<Resumed>,
}

struct Resumed {
    /// User and assistant messages in the model's context.
    messages: usize,
    /// When the last record was written.
    last_activity_ms: Option<u64>,
}

struct Built {
    session: AgentSession,
    provider: Arc<dyn Provider>,
    about: About,
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
    let about = About {
        profile: std::mem::take(&mut prepared.profile_name),
        tools: prepared
            .config
            .spec
            .tools
            .iter()
            .map(|tool| tool.name().to_string())
            .collect(),
        auto_approve: std::mem::take(&mut prepared.auto_approve_commands),
        cwd: prepared.config.spec.cwd.clone(),
        session: prepared.store.path().map(Path::to_path_buf),
        resumed: (!prepared.store.entries().is_empty()).then(|| Resumed {
            messages: prepared
                .store
                .messages()
                .iter()
                .filter(|message| matches!(message, Message::User { .. } | Message::Assistant(_)))
                .count(),
            last_activity_ms: prepared.store.entries().last().map(|entry| entry.timestamp()),
        }),
    };
    let session = prepared.into_session(output.clone())?;
    let traced = settings.traces.clone().map(|sink| session.subscribe(sink));
    Ok(Built {
        session,
        provider,
        about,
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
    let output = Arc::new(Output::new(
        args.output,
        args.deltas,
        args.debug,
        args.color,
        args.show_thinking,
    ));
    let mut model = args.base.model.clone();
    let mut reasoning = args.request.reasoning;
    let storage = if args.no_session {
        Storage::Memory
    } else if let Some(path) = &args.request.session {
        Storage::File(path.clone())
    } else if let Some(dir) = &args.session_dir {
        Storage::Directory(dir.clone())
    } else {
        Storage::Directory(sessions::default_dir(&sessions::resolve_cwd(
            args.base.cwd.as_deref(),
        )?)?)
    };
    let (resumed, notice) = match (&storage, &args.resume, &args.fork) {
        (Storage::Directory(dir), _, Some(source)) => {
            let cwd = sessions::resolve_cwd(args.base.cwd.as_deref())?;
            let source = fork_source(dir, source).await?;
            let forked = sessions::fork(&source, dir, &cwd, &model, sessions::Branch::Last).await?;
            let notice = forked_line(&forked);
            (Some(forked.path), Some(notice))
        }
        (Storage::Directory(dir), Some(id), None) => {
            resume_target(dir, args.base.cwd.as_deref(), id.as_deref()).await?
        }
        (Storage::Memory | Storage::File(_) | Storage::Directory(_), _, _) => (None, None),
    };
    let file = match (&storage, &resumed) {
        (_, Some(path)) => SessionFile::Open(path),
        (Storage::Memory, None) => SessionFile::Memory,
        (Storage::File(path), None) => SessionFile::Open(path),
        (Storage::Directory(dir), None) => SessionFile::NewIn(dir),
    };
    let mut built = build(&settings, &model, file, &interaction, &output).await?;
    let mut signals = Signals::new().context("cannot register chat signal handlers")?;
    let mut input = input::spawn(mode)?;
    if let Some(notice) = notice {
        output.line(Tone::Dim, &notice);
    }
    banner(&output, &model, &built.about);
    let mut next_prompt: Option<String> = None;
    let mut last_interrupt: Option<Instant> = None;

    loop {
        if mode == InputMode::Terminal {
            output.prompt(PROMPT);
        }
        let line = if let Some(text) = next_prompt.take() {
            if mode == InputMode::Terminal {
                output.echo(&visible(&text));
            }
            Line::Text(text)
        } else {
            tokio::select! {
                biased;
                interrupt = signals.next() => {
                    match interrupt {
                        Interrupt::Cancel => {
                            if last_interrupt.is_some_and(|at| at.elapsed() <= QUIT_INTERRUPT_WINDOW) {
                                output.finish();
                                return Ok(EXIT_COMPLETED);
                            }
                            last_interrupt = Some(Instant::now());
                            output.line(Tone::Dim, "[chat] no active task; press Ctrl+C again to exit, or /quit");
                            continue;
                        }
                        Interrupt::Terminate => { output.finish(); return Ok(EXIT_ABORTED); }
                    }
                }
                line = input.recv() => match line {
                    Some(line) => {
                        last_interrupt = None;
                        if mode == InputMode::Terminal { output.input_echoed(); }
                        line.context("cannot read terminal input")?
                    }
                    None => { output.finish(); return Ok(EXIT_COMPLETED); }
                },
            }
        };
        let mut io = TurnIo {
            input: &mut input,
            approvals: &mut approvals,
            signals: &mut signals,
            mode,
            output: &output,
        };
        let line = match line {
            Line::Text(text) => text,
            Line::Rejected(reason) => {
                rejected(&output, &reason);
                continue;
            }
        };
        let action = match commands::parse(line) {
            Command::Prompt(text) => drive_turn(&built.session, Operation::Prompt(text), &mut io).await?,
            Command::Compact => drive_turn(&built.session, Operation::Compact, &mut io).await?,
            Command::Continue => drive_turn(&built.session, Operation::Continue, &mut io).await?,
            Command::New => Flow::New,
            Command::Model(None) => {
                output.line(Tone::Dim, &format!("[chat] model={}", visible(&model)));
                Flow::Continue
            }
            Command::Reasoning(None) => {
                output.line(Tone::Dim, &format!("[chat] reasoning={}", reasoning_name(reasoning)));
                Flow::Continue
            }
            Command::Reasoning(Some(next)) => match parse_level(&next) {
                Ok(level) => {
                    built.session.set_reasoning(level)?;
                    reasoning = level;
                    output.line(Tone::Dim, &format!("[chat] reasoning={}", reasoning_name(reasoning)));
                    Flow::Continue
                }
                Err(message) => {
                    output.line(Tone::Warn, &format!("[chat] {}", visible(&message)));
                    Flow::Continue
                }
            },
            Command::Model(Some(next)) => {
                built.session.set_model(ModelRef {
                    provider: built.provider.clone(),
                    model: next.clone(),
                })?;
                model = next;
                output.line(Tone::Dim, &format!("[chat] model={}", visible(&model)));
                Flow::Continue
            }
            Command::Status => {
                status(&output, &model, &built.about);
                Flow::Continue
            }
            Command::Sessions => {
                list_sessions(&output, &storage, &built.about).await;
                Flow::Continue
            }
            Command::Resume(id) => match find_session(&storage, &id, &built.about).await {
                Ok(path) => Flow::Resume(path),
                Err(error) => {
                    output.line(Tone::Warn, &format!("[chat] {}", visible(&format!("{error:#}"))));
                    Flow::Continue
                }
            },
            Command::Fork => match fork_current(&storage, &built.about, &model, &built.session).await {
                Ok(forked) => {
                    output.line(Tone::Dim, &forked_line(&forked));
                    Flow::Resume(forked.path)
                }
                Err(error) => {
                    output.line(Tone::Warn, &format!("[chat] {}", visible(&format!("{error:#}"))));
                    Flow::Continue
                }
            },
            Command::Tree => {
                show_tree(&output, &built.session);
                Flow::Continue
            }
            Command::Branch(id) => {
                switch_branch(&output, &built.session, &id);
                Flow::Continue
            }
            Command::Quit => Flow::Exit(EXIT_COMPLETED),
            Command::Help => {
                help(&output);
                Flow::Continue
            }
            Command::Invalid(message) => {
                output.line(Tone::Warn, &format!("[chat] {}", visible(&message)));
                Flow::Continue
            }
            Command::Empty => Flow::Continue,
        };
        match action {
            Flow::Continue => {}
            Flow::Prompt(text) => next_prompt = Some(text),
            Flow::Exit(code) => {
                output.finish();
                return Ok(code);
            }
            Flow::New => {
                let sibling;
                let file = match &storage {
                    Storage::Memory => SessionFile::Memory,
                    Storage::File(original) => {
                        sibling = new_file(original);
                        SessionFile::Create(&sibling)
                    }
                    Storage::Directory(dir) => SessionFile::NewIn(dir),
                };
                match build(&settings, &model, file, &interaction, &output).await {
                    Ok(next) => {
                        built = next;
                        output.reset_totals();
                        let file = built
                            .about
                            .session
                            .as_deref()
                            .map(|path| format!("; session={}", path.display()))
                            .unwrap_or_default();
                        output.line(Tone::Dim, &format!("[chat] new conversation{file}"));
                    }
                    Err(error) => output.line(
                        Tone::Error,
                        &format!(
                            "[chat] cannot create new conversation: {}",
                            visible(&format!("{error:#}"))
                        ),
                    ),
                }
            }
            Flow::Resume(path) => match build(&settings, &model, SessionFile::Open(&path), &interaction, &output).await
            {
                Ok(next) => {
                    built = next;
                    output.reset_totals();
                    output.line(
                        Tone::Dim,
                        &format!("[chat] session={}", visible(&path.display().to_string())),
                    );
                    resumed_line(&output, &built.about);
                }
                Err(error) => output.line(
                    Tone::Error,
                    &format!("[chat] cannot resume the session: {}", visible(&format!("{error:#}"))),
                ),
            },
        }
    }
}

/// The session `--resume` opens, and a line to show when there is none to resume.
async fn resume_target(dir: &Path, cwd: Option<&Path>, id: Option<&str>) -> Result<(Option<PathBuf>, Option<String>)> {
    let list = sessions::list(dir.to_path_buf()).await?;
    match id {
        Some(id) => {
            let session = list
                .find(id)
                .with_context(|| format!("--resume: no single session to resume in {}", dir.display()))?;
            Ok((Some(session.path.clone()), None))
        }
        None => {
            let cwd = sessions::resolve_cwd(cwd)?;
            Ok(match list.latest_in(&cwd) {
                Some(session) => (Some(session.path.clone()), None),
                None => (
                    None,
                    Some(format!(
                        "[chat] no earlier session of this directory in {}; starting a new one",
                        visible(&dir.display().to_string())
                    )),
                ),
            })
        }
    }
}

/// The session file `--fork` copies: a file, or else the session whose ID starts with
/// `value` in the session directory.
async fn fork_source(dir: &Path, value: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    let list = sessions::list(dir.to_path_buf()).await?;
    let session = list.find(value).with_context(|| {
        format!(
            "--fork: {value} is neither a session file nor a session in {}",
            dir.display()
        )
    })?;
    Ok(session.path.clone())
}

/// /fork: copy the current session, up to its current position, into a new file of the
/// session directory.
async fn fork_current(
    storage: &Storage,
    about: &About,
    model: &str,
    session: &AgentSession,
) -> Result<sessions::Forked> {
    let (Some(dir), Some(current)) = (storage.dir(), about.session.as_deref()) else {
        anyhow::bail!("nothing to fork: the conversation is kept in memory");
    };
    // The position /branch moved is only in memory; the file's last record may be elsewhere.
    let leaf = session.inspect(|store| store.leaf_id().map(str::to_string))?;
    sessions::fork(current, dir, &about.cwd, model, sessions::Branch::At(leaf.as_deref())).await
}

/// /tree: the session tree, one line per message or summary.
fn show_tree(output: &Output, session: &AgentSession) {
    match session.inspect(tree::render) {
        Ok(lines) => {
            for line in lines {
                output.line(Tone::Plain, &line);
            }
        }
        Err(error) => output.line(Tone::Warn, &format!("[chat] cannot show the tree: {error}")),
    }
}

/// /branch: continue after the entry named, or before the user's own message named.
fn switch_branch(output: &Output, session: &AgentSession, id: &str) {
    let found = session.inspect(|store| (tree::resolve(store, id), store.leaf_id().map(str::to_string)));
    let (target, leaf) = match found {
        Ok((Ok(target), leaf)) => (target, leaf),
        Ok((Err(message), _)) => {
            output.line(Tone::Warn, &format!("[chat] {message}"));
            return;
        }
        Err(error) => {
            output.line(Tone::Warn, &format!("[chat] cannot switch branch: {error}"));
            return;
        }
    };
    let (next, line) = match target {
        Target::After { id, line } => (
            Some(id.clone()),
            format!("[chat] continuing after {id} ({line}); the next message starts a new branch there"),
        ),
        Target::Before { parent, id, prompt } => (
            parent,
            format!("[chat] continuing before your message {id} ({prompt}); the next message takes its place"),
        ),
    };
    if next == leaf {
        output.line(Tone::Dim, "[chat] already at that point");
        return;
    }
    match session.branch(next.as_deref()) {
        Ok(()) => output.line(Tone::Dim, &line),
        Err(error) => output.line(Tone::Warn, &format!("[chat] cannot switch branch: {error}")),
    }
}

fn forked_line(forked: &sessions::Forked) -> String {
    let short: String = forked.source_id.chars().take(sessions::SHORT_ID_CHARS).collect();
    format!(
        "[chat] forked session {} ({} record(s)) into {}",
        visible(&short),
        forked.entries,
        visible(&forked.path.display().to_string())
    )
}

/// /sessions: the sessions of the session directory, the current one marked with `*`.
async fn list_sessions(output: &Output, storage: &Storage, about: &About) {
    let Some(dir) = storage.dir() else {
        output.line(
            Tone::Warn,
            "[chat] no session directory: the conversation is kept in memory",
        );
        return;
    };
    let list = match sessions::list(dir.to_path_buf()).await {
        Ok(list) => list,
        Err(error) => {
            output.line(Tone::Warn, &format!("[chat] {}", visible(&format!("{error:#}"))));
            return;
        }
    };
    let lines = sessions::lines(&list, &about.cwd);
    for (session, line) in list.sessions.iter().zip(lines) {
        let mark = if about.session.as_deref() == Some(session.path.as_path()) {
            "* "
        } else {
            "  "
        };
        output.line(Tone::Plain, &format!("{mark}{line}"));
    }
    for unreadable in &list.unreadable {
        output.line(
            Tone::Warn,
            &format!(
                "[chat] cannot read {}: {}",
                visible(&unreadable.path.display().to_string()),
                visible(&unreadable.error)
            ),
        );
    }
    output.line(
        Tone::Dim,
        &format!(
            "[chat] {} session(s) in {}; /resume <ID> switches",
            list.sessions.len(),
            visible(&dir.display().to_string())
        ),
    );
}

/// The file of the session /resume names, unless it is the current one.
async fn find_session(storage: &Storage, id: &str, about: &About) -> Result<PathBuf> {
    let dir = storage
        .dir()
        .context("no session directory: the conversation is kept in memory")?;
    let list = sessions::list(dir.to_path_buf()).await?;
    let path = list.find(id)?.path.clone();
    anyhow::ensure!(
        about.session.as_deref() != Some(path.as_path()),
        "already in session {id}"
    );
    Ok(path)
}

struct TurnIo<'a> {
    input: &'a mut input::Input,
    approvals: &'a mut mpsc::Receiver<Ticket>,
    signals: &'a mut Signals,
    mode: InputMode,
    output: &'a Output,
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
    fn print(&self, output: &Output, elapsed: Duration) {
        match self {
            Self::Task(report) => output.report(report, elapsed),
            Self::Compact(report) => output.compact_report(report),
        }
    }
}

/// A menu choice while a call waits for approval.
enum Choice {
    Once,
    Session,
    Deny,
    Discuss,
}

fn approval_choice(text: &str) -> Option<Choice> {
    match text.trim().to_ascii_lowercase().as_str() {
        "1" | "y" | "yes" => Some(Choice::Once),
        "2" => Some(Choice::Session),
        "3" | "n" | "no" => Some(Choice::Deny),
        "4" => Some(Choice::Discuss),
        _ => None,
    }
}

async fn drive_turn(session: &AgentSession, operation: Operation, io: &mut TurnIo<'_>) -> Result<Flow> {
    let compact = matches!(operation, Operation::Compact);
    let started = Instant::now();
    let running = async {
        match operation {
            Operation::Prompt(text) => session.prompt(text).await.map(Report::Task),
            Operation::Continue => session.continue_run().await.map(Report::Task),
            Operation::Compact => session.compact().await.map(Report::Compact),
        }
    };
    tokio::pin!(running);
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // A status an earlier operation left behind must not tick on into this one.
    io.output.clear_status();
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
                report.print(io.output, started.elapsed());
                if queued_prompt.is_some() { io.output.line(Tone::Dim, "[chat] queued input cleared by cancellation"); }
                // Steering meant for the cancelled task must not reach the next one.
                let dropped = unread_input(session, io.output);
                if !dropped.is_empty() {
                    io.output.line(Tone::Dim, &format!(
                        "[chat] cancellation discarded {} steering input(s): {}",
                        dropped.len(),
                        visible(&dropped.join(" / "))
                    ));
                }
                return Ok(match interrupt {
                    Interrupt::Cancel => Flow::Continue,
                    Interrupt::Terminate => Flow::Exit(EXIT_ABORTED),
                });
            }
            report = &mut running => {
                report?.print(io.output, started.elapsed());
                // Steering that arrived as the task ended was never read: ask it next.
                let late = unread_input(session, io.output);
                if !late.is_empty() {
                    io.output.line(Tone::Dim, "[chat] steering input arrived as the task ended; sending it as the next prompt");
                }
                let next: Vec<String> = queued_prompt.into_iter().chain(late).collect();
                return Ok(if next.is_empty() { Flow::Continue } else { Flow::Prompt(next.join("\n\n")) });
            }
            () = expired(&mut pending), if pending.is_some() => {
                discussing = false;
                if let Some(ticket) = pending.take() {
                    io.output.line(Tone::Warn, &format!("[approval] request {} expired; the task goes on without the call", ticket.id));
                }
            }
            request = io.approvals.recv() => {
                let Some(ticket) = request else { anyhow::bail!("terminal approval channel closed"); };
                if ticket.reply.is_closed() { continue; }
                if let Some(previous) = pending.take() {
                    // Defensive fail-closed behavior; the core normally asks serially.
                    deny(io.output, previous, "superseded terminal approval");
                }
                discussing = false;
                show_approval(io.output, &ticket);
                pending = Some(ticket);
            }
            line = io.input.recv(), if io.mode == InputMode::Terminal || pending.is_some() => {
                if io.mode == InputMode::Terminal && line.is_some() { io.output.input_echoed(); }
                let command = match line {
                    Some(line) => match line.context("cannot read terminal input")? {
                        Line::Text(text) => {
                            if pending.is_some() && text.trim() != "/quit" && text.trim() != "/new" {
                                if discussing {
                                    if text.trim().is_empty() {
                                        io.output.line(Tone::Warn, "[approval] enter a message to discuss this call");
                                    } else if let Some(ticket) = pending.take() {
                                        session.steer(text);
                                        deny(io.output, ticket, "user wants to discuss this call");
                                        discussing = false;
                                    }
                                } else {
                                    match approval_choice(&text) {
                                        Some(choice @ (Choice::Once | Choice::Session)) => {
                                            let decision = match choice {
                                                Choice::Once => Approval::AllowOnce,
                                                _ => Approval::AllowSession,
                                            };
                                            if let Some(ticket) = pending.take()
                                                && ticket.reply.send(decision).is_err() {
                                                io.output.line(Tone::Warn, "[approval] request expired");
                                            }
                                        }
                                        Some(Choice::Deny) => { if let Some(ticket) = pending.take() { deny(io.output, ticket, "declined by the user"); } }
                                        Some(Choice::Discuss) => { discussing = true; io.output.line(Tone::Warn, "[approval] What would you like to discuss? The call will not run."); }
                                        None => io.output.line(Tone::Warn, "[approval] choose 1, 2, 3 or 4"),
                                    }
                                }
                                continue;
                            }
                            commands::parse(text)
                        },
                        Line::Rejected(reason) => {
                            rejected(io.output, &reason);
                            continue;
                        }
                    },
                    None => {
                        if let Some(ticket) = pending.take() { deny(io.output, ticket, "terminal input closed"); }
                        session.abort();
                        running.await?.print(io.output, started.elapsed());
                        return Ok(Flow::Exit(EXIT_COMPLETED));
                    }
                };
                match command {
                    Command::Quit | Command::New => {
                        let next = if matches!(command, Command::New) { Flow::New } else { Flow::Exit(EXIT_COMPLETED) };
                        session.abort();
                        drop(pending.take());
                        running.await?.print(io.output, started.elapsed());
                        return Ok(next);
                    }
                    Command::Prompt(text) if compact => {
                        if queued_prompt.is_none() {
                            queued_prompt = Some(text);
                            io.output.line(Tone::Dim, "[chat] input queued after compaction");
                        } else { io.output.line(Tone::Warn, "[chat] one input is already queued; wait for compaction or cancel it"); }
                    }
                    Command::Prompt(text) if pending.is_none() => {
                        session.steer(text);
                        io.output.line(Tone::Dim, "[chat] steering input queued");
                    }
                    Command::Prompt(_) => unreachable!("pending approval input is handled above"),
                    Command::Model(_) => io.output.line(Tone::Warn, "[chat] wait for the current task or cancel it before changing models"),
                    Command::Reasoning(_) => io.output.line(Tone::Warn, "[chat] wait for the current task or cancel it before changing the reasoning level"),
                    Command::Continue => io.output.line(Tone::Warn, "[chat] session is busy; wait for the current operation or cancel it before /continue"),
                    Command::Status => io.output.line(Tone::Warn, "[chat] wait for the current task or cancel it before /status"),
                    Command::Sessions => io.output.line(Tone::Warn, "[chat] wait for the current task or cancel it before /sessions"),
                    Command::Resume(_) => io.output.line(Tone::Warn, "[chat] wait for the current task or cancel it before /resume"),
                    Command::Fork => io.output.line(Tone::Warn, "[chat] wait for the current task or cancel it before /fork"),
                    Command::Tree => io.output.line(Tone::Warn, "[chat] wait for the current task or cancel it before /tree"),
                    Command::Branch(_) => io.output.line(Tone::Warn, "[chat] wait for the current task or cancel it before /branch"),
                    Command::Help => help(io.output),
                    Command::Compact => io.output.line(Tone::Warn, "[chat] session is busy; wait for the current operation or cancel it before /compact"),
                    Command::Invalid(message) => io.output.line(Tone::Warn, &format!("[chat] {}", visible(&message))),
                    Command::Empty => {}
                }
            }
            _ = tick.tick() => io.output.tick(),
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
fn unread_input(session: &AgentSession, output: &Output) -> Vec<String> {
    session.take_queued().unwrap_or_else(|error| {
        output.line(
            Tone::Error,
            &format!("[chat] cannot take the unread input: {}", visible(&error.to_string())),
        );
        Vec::new()
    })
}

fn show_approval(output: &Output, ticket: &Ticket) {
    let call = &ticket.request.call;
    let style = output.style();
    // The label is colored, the call itself is bold in the default color: it is what a
    // person has to read.
    output.line(
        Tone::Plain,
        &format!(
            "{} {}",
            style.paint(Tone::Warn, &format!("[approval {}]", ticket.id)),
            style.paint(Tone::Strong, &output::describe_call(&call.name, &call.arguments))
        ),
    );
    for line in output::describe_arguments(&call.arguments) {
        output.line(Tone::Plain, &format!("  {line}"));
    }
    output.line(Tone::Dim, &format!("  {}", visible(&ticket.request.reason)));
    output.line(Tone::Dim, APPROVAL_MENU);
    output.line(Tone::Warn, APPROVAL_PROMPT);
}

/// A discarded input line ends neither the conversation nor the current task.
fn rejected(output: &Output, reason: &str) {
    output.line(Tone::Warn, &format!("[chat] input rejected: {}", visible(reason)));
}

fn deny(output: &Output, ticket: Ticket, reason: &str) {
    if ticket.reply.send(Approval::Deny(reason.into())).is_err() {
        // The runtime already stopped waiting; no call is authorized.
        output.line(Tone::Warn, &format!("[approval] request {} expired", ticket.id));
    }
}

/// For /new with `--session`: a new file beside the original.
fn new_file(original: &Path) -> PathBuf {
    let mut name = original
        .file_stem()
        .unwrap_or_else(|| OsStr::new("session"))
        .to_os_string();
    name.push(format!(".{}.jsonl", uuid::Uuid::new_v4()));
    original.with_file_name(name)
}

fn session_text(about: &About) -> String {
    about
        .session
        .as_deref()
        .map_or_else(|| "memory".to_string(), |path| path.display().to_string())
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

/// What the conversation runs on, once at startup.
/// How much of an earlier conversation the session file held, when it held one.
fn resumed_line(output: &Output, about: &About) {
    let Some(resumed) = &about.resumed else {
        return;
    };
    let when = resumed
        .last_activity_ms
        // A timestamp beyond i64 cannot occur; without one the line just omits the time.
        .and_then(|ms| i64::try_from(ms).ok())
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|utc| {
            format!(
                "; last activity {}",
                utc.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M")
            )
        })
        .unwrap_or_default();
    output.line(
        Tone::Dim,
        &format!(
            "[chat] resumed {} message(s) from the session file{when}",
            resumed.messages
        ),
    );
}

fn banner(output: &Output, model: &str, about: &About) {
    output.line(
        Tone::Dim,
        &format!(
            "[chat] model={} · profile={} · session={} · cwd={}",
            visible(model),
            visible(&about.profile),
            visible(&session_text(about)),
            visible(&about.cwd.display().to_string())
        ),
    );
    resumed_line(output, about);
    let approvals = if about.tools.iter().any(|tool| tool == BASH) {
        let asks = if about.auto_approve.is_empty() {
            "every bash command asks first".to_string()
        } else {
            format!("bash asks first, except {}", visible(&about.auto_approve.join(", ")))
        };
        format!(" · {asks} · commands run with this process's permissions")
    } else {
        String::new()
    };
    output.line(
        Tone::Dim,
        &format!(
            "[chat] tools={}{approvals} · /help for commands",
            visible(&about.tools.join(","))
        ),
    );
}

fn status(output: &Output, model: &str, about: &About) {
    output.line(
        Tone::Dim,
        &format!(
            "[status] model={} · profile={} · session={} · cwd={}",
            visible(model),
            visible(&about.profile),
            visible(&session_text(about)),
            visible(&about.cwd.display().to_string())
        ),
    );
    let totals = output.totals();
    output.line(
        Tone::Dim,
        &format!(
            "[status] this conversation: {} turn(s) · {} request(s) · {} in · {} out · {} cached",
            totals.turns, totals.requests, totals.input_tokens, totals.output_tokens, totals.cached_tokens
        ),
    );
}

fn help(output: &Output) {
    for line in [
        "/new                  start a fresh conversation; approvals granted so far are dropped",
        "/model [id]           show or change the model while idle",
        "/reasoning [level]    show or change the reasoning level while idle: off, minimal,",
        "                      low, medium, high, xhigh, or default to leave it to the server",
        "/compact              summarize old context while retaining raw history",
        "/continue             continue without new input, such as after a failed answer",
        "/status               show the model, session file and what this conversation has used",
        "/sessions             list the sessions in the session directory, most recent first",
        "/resume <ID>          switch to the session whose ID starts with <ID>",
        "/fork                 continue in a copy of this session; the original stays as it is",
        "/tree                 show the session tree: every message on every branch, the current one marked",
        "/branch <ID>          continue after the entry with that ID, or before your own message with that ID",
        "/quit                 exit chat; Ctrl+D while idle does the same",
        "Ctrl+C                cancel the current task or approval; keep the conversation",
        "                      twice within 2 seconds while idle: exit",
        "While a task runs, a typed line is steering: the model sees it before its next request.",
        "Lines pasted together are sent as one message; a slash command is always one line.",
        "Approvals: 1 or y allow once · 2 allow identical calls in this conversation · 3 or n deny · 4 discuss",
    ] {
        output.line(Tone::Plain, line);
    }
}
