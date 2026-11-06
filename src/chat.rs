//! The line-oriented terminal frontend; model and tool work stays in AgentSession. Where
//! conversations are kept is in `storage`, a running operation in `turn`, the banner,
//! `/status` and `/help` in `info`.

mod commands;
mod debug;
mod info;
mod input;
mod interaction;
mod output;
mod signals;
mod storage;
mod term;
mod tree;
mod turn;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use zx_agent::AgentSession;
use zx_agent::agent::{EventSink, QueueMode, QueuedMessage};
use zx_agent::agent_session::{ModelRef, Subscription};
use zx_agent::ai::{Provider, Reasoning};
use zx_agent::otel::Telemetry;
use zx_agent::profile::Builtin;

use crate::cli::{self, ChatArgs, ModeSettings, SessionOptions};
use crate::process::{EXIT_ABORTED, EXIT_COMPLETED, with_telemetry};
use crate::render::visible;
use crate::sessions::SessionFile;

use commands::Command;
use info::{About, banner, help, resumed_line, status};
use input::Line;
use interaction::TerminalInteraction;
use output::Output;
use signals::{Interrupt, Signals};
use storage::{Storage, find_session, fork_current, forked_line, list_sessions, new_file, opening};
use term::Tone;
use tree::Target;
use turn::{Operation, TurnIo, drive_turn};

/// A second Ctrl+C within this long, while idle, exits.
const QUIT_INTERRUPT_WINDOW: Duration = Duration::from_secs(2);
const PROMPT: &str = "zx> ";

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

struct Built {
    session: AgentSession,
    provider: Arc<dyn Provider>,
    about: About,
    /// The commands extensions registered: name and description, for parsing and /help.
    commands: Vec<(String, String)>,
    /// The trace export of this session, while it lasts.
    _traced: Option<Subscription>,
}

/// What every conversation of this chat is built with.
struct Chat {
    settings: ModeSettings<ChatArgs>,
    interaction: Arc<TerminalInteraction>,
    output: Arc<Output>,
}

impl Chat {
    async fn build(&self, model: &str, file: SessionFile<'_>) -> Result<Built> {
        let settings = &self.settings;
        let args = &settings.args;
        let interaction = &self.interaction;
        let output = &self.output;
        let mut prepared = cli::prepare(SessionOptions {
            base: &args.base,
            transport: &args.transport,
            request: &args.request,
            budget: &args.budget,
            model,
            cwd: None,
            file,
            default_profile: Builtin::Assistant,
            continue_on_length: None,
            shared: &settings.shared,
            hidden_env: &[],
        })
        .await?;
        prepared.require_bash_approval("shell commands require interactive approval");
        prepared.config.interaction = interaction.clone();
        prepared.config.interaction_timeout = std::time::Duration::from_secs(settings.args.approval_timeout_secs);
        let provider = prepared.config.spec.provider.clone();
        let about = About::of(&mut prepared);
        let session = prepared.into_session(output.clone())?;
        if let Some(mode) = args.queue_mode {
            session.queues().set_steering_mode(mode);
            session.queues().set_follow_up_mode(mode);
        }
        let commands = session
            .commands()
            .iter()
            .map(|command| (command.name().to_string(), command.description().to_string()))
            .collect();
        let traced = settings.traces.clone().map(|sink| session.subscribe(sink));
        Ok(Built {
            session,
            provider,
            about,
            commands,
            _traced: traced,
        })
    }

    /// Replace the conversation with one on `file`; when that fails, the current one stays
    /// and `what` says what could not be done.
    async fn switch(&self, built: &mut Built, model: &str, file: SessionFile<'_>, what: &str) -> bool {
        match self.build(model, file).await {
            Ok(next) => {
                *built = next;
                self.output.reset_totals();
                true
            }
            Err(error) => {
                self.output.line(
                    Tone::Error,
                    &format!("[chat] cannot {what}: {}", visible(&format!("{error:#}"))),
                );
                false
            }
        }
    }
}

pub(crate) async fn run(args: ChatArgs) -> Result<u8> {
    with_telemetry(async move |telemetry| converse(args, telemetry.map(Telemetry::sink)).await).await
}

#[expect(
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    reason = "chat is frozen; the REPL stays as it is"
)]
async fn converse(args: ChatArgs, traces: Option<Arc<dyn EventSink>>) -> Result<u8> {
    let shared = cli::Shared::connect(&args.base, Builtin::Assistant, &args.transport).await?;
    let mode = if std::io::stdin().is_terminal() {
        InputMode::Terminal
    } else {
        InputMode::Script
    };
    let (interaction, mut approvals) = TerminalInteraction::channel();
    let chat = Chat {
        output: Arc::new(Output::new(
            args.output,
            args.deltas,
            args.debug,
            args.color,
            args.show_thinking,
        )),
        interaction: Arc::new(interaction),
        settings: ModeSettings { args, shared, traces },
    };
    let args = &chat.settings.args;
    let output = &chat.output;
    let mut model = args.base.model.clone();
    let mut reasoning = args.request.reasoning;
    let (storage, first) = opening(args, &model).await?;
    let file = match (&storage, first.path()) {
        (_, Some(path)) | (Storage::File(path), None) => SessionFile::Open(path),
        (Storage::Memory, None) => SessionFile::Memory,
        (Storage::Directory(dir), None) => SessionFile::NewIn(dir),
    };
    let mut built = chat.build(&model, file).await?;
    let mut signals = Signals::new().context("cannot register chat signal handlers")?;
    let mut input = input::spawn(mode)?;
    if let Some(notice) = first.notice() {
        output.line(Tone::Dim, &notice);
    }
    banner(output, &model, &built.about);
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
            output,
            commands: &built.commands,
        };
        let line = match line {
            Line::Text(text) => text,
            Line::Rejected(reason) => {
                rejected(output, &reason);
                continue;
            }
        };
        let action = match commands::parse(line, &built.commands) {
            Command::Prompt(text) => drive_turn(&built.session, Operation::Prompt(text), &mut io).await?,
            Command::Extension { name, args } => {
                match run_extension_command(&built.session, &name, &args, output).await {
                    Some(message) => drive_turn(&built.session, Operation::Message(message), &mut io).await?,
                    None => Flow::Continue,
                }
            }
            Command::Queue(None) => {
                output.line(Tone::Dim, &queue_line(&built.session));
                Flow::Continue
            }
            Command::Queue(Some(mode)) => {
                set_queue_mode(&built.session, &mode, output);
                Flow::Continue
            }
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
                status(output, &model, &built.about);
                Flow::Continue
            }
            Command::Sessions => {
                list_sessions(output, &storage, &built.about).await;
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
                show_tree(output, &built.session);
                Flow::Continue
            }
            Command::Branch(id) => {
                switch_branch(output, &built.session, &id);
                Flow::Continue
            }
            Command::Quit => Flow::Exit(EXIT_COMPLETED),
            Command::Help => {
                help(output, &built.commands);
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
                if chat.switch(&mut built, &model, file, "create new conversation").await {
                    let file = built
                        .about
                        .session
                        .as_deref()
                        .map(|path| format!("; session={}", path.display()))
                        .unwrap_or_default();
                    output.line(Tone::Dim, &format!("[chat] new conversation{file}"));
                }
            }
            Flow::Resume(path) => {
                if chat
                    .switch(&mut built, &model, SessionFile::Open(&path), "resume the session")
                    .await
                {
                    output.line(
                        Tone::Dim,
                        &format!("[chat] session={}", visible(&path.display().to_string())),
                    );
                    resumed_line(output, &built.about);
                }
            }
        }
    }
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
    let found = session.inspect(|store| (tree::target(store, id), store.leaf_id().map(str::to_string)));
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

/// Run an extension's command and show what it says. Returns the message it sends, if any.
async fn run_extension_command(
    session: &AgentSession,
    name: &str,
    args: &str,
    output: &Output,
) -> Option<QueuedMessage> {
    match session.run_command(name, args).await {
        Ok(outcome) => {
            for line in outcome.lines {
                output.line(Tone::Plain, &visible(&line));
            }
            outcome.prompt
        }
        Err(error) => {
            output.line(
                Tone::Warn,
                &format!("[chat] /{}: {}", visible(name), visible(&error.to_string())),
            );
            None
        }
    }
}

fn queue_line(session: &AgentSession) -> String {
    format!(
        "[chat] queue: steering={} follow-up={}",
        session.queues().steering_mode().as_str(),
        session.queues().follow_up_mode().as_str()
    )
}

/// /queue <mode>: take queued input all at once or one message per request, both queues.
fn set_queue_mode(session: &AgentSession, mode: &str, output: &Output) {
    match mode.parse::<QueueMode>() {
        Ok(mode) => {
            session.queues().set_steering_mode(mode);
            session.queues().set_follow_up_mode(mode);
            output.line(Tone::Dim, &queue_line(session));
        }
        Err(message) => output.line(Tone::Warn, &format!("[chat] {}", visible(&message))),
    }
}

/// A discarded input line ends neither the conversation nor the current task.
fn rejected(output: &Output, reason: &str) {
    output.line(Tone::Warn, &format!("[chat] input rejected: {}", visible(reason)));
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
