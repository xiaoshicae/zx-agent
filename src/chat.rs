//! The line-oriented terminal frontend; model and tool work stays in AgentSession.
//!
//! - `input`: the single owner of the terminal's input
//! - `turn`: one operation of a conversation driven to its end
//! - `interaction`: approval requests, answered through the input owner
//! - `signals`: signals that cancel a prompt and leave the REPL running
//! - `output` and `term`: streaming output and colors
//! - `commands`: slash commands, which never reach the model
//! - `storage`: where conversations are kept, and which one opens first
//! - `info`: the banner, `/status` and `/help`

mod commands;
mod info;
mod input;
mod interaction;
mod output;
mod signals;
mod storage;
mod term;
mod turn;

use std::io::IsTerminal;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::sync::mpsc;

use zx_agent::AgentSession;
use zx_agent::agent::{EventSink, QueuedMessage};
use zx_agent::agent_session::ModelRef;
use zx_agent::ai::{Provider, Reasoning};
use zx_agent::config::Builtin;
use zx_agent::otel::Telemetry;

use crate::cli::{self, ChatArgs};
use crate::process::{EXIT_ABORTED, EXIT_COMPLETED, with_telemetry};
use crate::render::visible;
use crate::session_files::SessionFile;

use commands::Command;
use info::{About, banner, help, status};
use input::Line;
use interaction::{TerminalInteraction, Ticket};
use output::Output;
use signals::{Interrupt, Signals};
use storage::{Storage, new_file, opening};
use term::Tone;
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
    Exit(u8),
}

struct Built {
    session: AgentSession,
    provider: Arc<dyn Provider>,
    about: About,
    /// The commands extensions registered: name and description, for parsing and /help.
    commands: Vec<(String, String)>,
}

/// What every conversation of this chat is built with.
struct Chat {
    shared: cli::Shared,
    interaction: Arc<TerminalInteraction>,
    output: Arc<Output>,
}

impl Chat {
    async fn build(&self, model: &str, file: SessionFile<'_>) -> Result<Built> {
        let mut prepared = cli::prepare(&self.shared, model, None, file).await?;
        prepared.require_bash_approval("shell commands require interactive approval");
        prepared.config.interaction = self.interaction.clone();
        prepared.config.interaction_timeout = self.shared.settings.approval_timeout;
        let provider = prepared.config.provider.clone();
        let about = About::of(&mut prepared);
        let session = prepared.into_session(self.output.clone())?;
        let commands = session
            .commands()
            .iter()
            .map(|command| (command.name().to_string(), command.description().to_string()))
            .collect();
        Ok(Built {
            session,
            provider,
            about,
            commands,
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

async fn converse(args: ChatArgs, traces: Option<Arc<dyn EventSink>>) -> Result<u8> {
    let ChatArgs {
        base,
        budget,
        output: output_mode,
        deltas,
        color,
        session,
        no_session,
        resume,
    } = args;
    let shared = cli::Shared::connect(base, budget, Builtin::Assistant)
        .await?
        .with_traces(traces);
    let mode = if std::io::stdin().is_terminal() {
        InputMode::Terminal
    } else {
        InputMode::Script
    };
    let (interaction, approvals) = TerminalInteraction::channel();
    let chat = Chat {
        output: Arc::new(Output::new(output_mode, deltas, color)),
        interaction: Arc::new(interaction),
        shared,
    };
    let output = &chat.output;
    let model = chat.shared.settings.model.clone();
    let resume = resume.as_ref().map(Option::as_deref);
    let (storage, first) = opening(no_session, session, resume, chat.shared.base.cwd.as_deref()).await?;
    let file = match (&storage, first.path()) {
        (_, Some(path)) | (Storage::File(path), None) => SessionFile::Open(path),
        (Storage::Memory, None) => SessionFile::Memory,
        (Storage::Directory(dir), None) => SessionFile::NewIn(dir),
    };
    let built = chat.build(&model, file).await?;
    let mut terminal = Terminal {
        signals: Signals::new().context("cannot register chat signal handlers")?,
        input: input::spawn(mode)?,
        approvals,
        mode,
        next_prompt: None,
        last_interrupt: None,
    };
    if let Some(notice) = first.notice() {
        output.line(Tone::Dim, &notice);
    }
    banner(output, &model, &built.about);
    let mut repl = Repl {
        chat: &chat,
        storage,
        built,
        model,
        reasoning: chat.shared.settings.config.reasoning,
    };
    loop {
        let line = match terminal.read(output).await? {
            Read::Line(line) => line,
            Read::Again => continue,
            Read::Exit(code) => {
                output.finish();
                return Ok(code);
            }
        };
        let text = match line {
            Line::Text(text) => text,
            Line::Rejected(reason) => {
                rejected(output, &reason);
                continue;
            }
        };
        let command = commands::parse(text, &repl.built.commands);
        let flow = repl.command(command, &mut terminal).await?;
        if let Some(code) = repl.follow(flow, &mut terminal).await {
            output.finish();
            return Ok(code);
        }
    }
}

/// What a read of the terminal gave while no task runs.
enum Read {
    Line(Line),
    /// Nothing to do this time, such as after one Ctrl+C.
    Again,
    /// The chat ends with this exit code.
    Exit(u8),
}

/// The terminal side of the REPL: its input, approvals and signals, and what the next read
/// starts from.
struct Terminal {
    input: input::Input,
    approvals: mpsc::Receiver<Ticket>,
    signals: Signals,
    mode: InputMode,
    /// A prompt to send next without reading, such as input that arrived as a task ended.
    next_prompt: Option<String>,
    /// When Ctrl+C was last pressed while idle; a second one soon after exits.
    last_interrupt: Option<Instant>,
}

impl Terminal {
    /// Read the next line, or the prompt queued for it. Ctrl+C twice, SIGTERM or the end of
    /// input end the chat.
    async fn read(&mut self, output: &Output) -> Result<Read> {
        if self.mode == InputMode::Terminal {
            output.prompt(PROMPT);
        }
        if let Some(text) = self.next_prompt.take() {
            if self.mode == InputMode::Terminal {
                output.echo(&visible(&text));
            }
            return Ok(Read::Line(Line::Text(text)));
        }
        tokio::select! {
            biased;
            interrupt = self.signals.next() => Ok(match interrupt {
                Interrupt::Cancel => {
                    if self.last_interrupt.is_some_and(|at| at.elapsed() <= QUIT_INTERRUPT_WINDOW) {
                        Read::Exit(EXIT_COMPLETED)
                    } else {
                        self.last_interrupt = Some(Instant::now());
                        output.line(Tone::Dim, "[chat] no active task; press Ctrl+C again to exit, or /quit");
                        Read::Again
                    }
                }
                Interrupt::Terminate => Read::Exit(EXIT_ABORTED),
            }),
            line = self.input.recv() => match line {
                Some(line) => {
                    self.last_interrupt = None;
                    if self.mode == InputMode::Terminal {
                        output.input_echoed();
                    }
                    Ok(Read::Line(line.context("cannot read terminal input")?))
                }
                None => Ok(Read::Exit(EXIT_COMPLETED)),
            },
        }
    }

    /// The terminal as one operation uses it, with the commands extensions registered.
    fn io<'a>(&'a mut self, output: &'a Output, commands: &'a [(String, String)]) -> TurnIo<'a> {
        TurnIo {
            input: &mut self.input,
            approvals: &mut self.approvals,
            signals: &mut self.signals,
            mode: self.mode,
            output,
            commands,
        }
    }
}

/// The conversation side of the REPL: the conversation, the model and reasoning level it runs
/// with, and where conversations are kept.
struct Repl<'a> {
    chat: &'a Chat,
    storage: Storage,
    built: Built,
    model: String,
    reasoning: Option<Reasoning>,
}

impl Repl<'_> {
    /// Carry out `command` typed while no task runs.
    async fn command(&mut self, command: Command, terminal: &mut Terminal) -> Result<Flow> {
        let output = &*self.chat.output;
        let session = &self.built.session;
        Ok(match command {
            Command::Prompt(text) => self.operate(Operation::Prompt(text), terminal).await?,
            Command::Extension { name, args } => match run_extension_command(session, &name, &args, output).await {
                Some(message) => self.operate(Operation::Message(message), terminal).await?,
                None => Flow::Continue,
            },
            Command::Compact => self.operate(Operation::Compact, terminal).await?,
            Command::Continue => self.operate(Operation::Continue, terminal).await?,
            Command::New => Flow::New,
            Command::Model(None) => self.say(Tone::Dim, &format!("[chat] model={}", visible(&self.model))),
            Command::Model(Some(next)) => self.set_model(next)?,
            Command::Reasoning(None) => self.say(
                Tone::Dim,
                &format!("[chat] reasoning={}", reasoning_name(self.reasoning)),
            ),
            Command::Reasoning(Some(next)) => self.set_reasoning(&next)?,
            Command::Status => {
                status(output, &self.model, &self.built.about);
                Flow::Continue
            }
            Command::Quit => Flow::Exit(EXIT_COMPLETED),
            Command::Help => {
                help(output, &self.built.commands);
                Flow::Continue
            }
            Command::Invalid(message) => self.say(Tone::Warn, &format!("[chat] {}", visible(&message))),
            Command::Empty => Flow::Continue,
        })
    }

    /// Run `operation` while the terminal takes steering, approvals and signals.
    async fn operate(&self, operation: Operation, terminal: &mut Terminal) -> Result<Flow> {
        let mut io = terminal.io(&self.chat.output, &self.built.commands);
        drive_turn(&self.built.session, operation, &mut io).await
    }

    /// Print `text` and go on.
    fn say(&self, tone: Tone, text: &str) -> Flow {
        self.chat.output.line(tone, text);
        Flow::Continue
    }

    /// /model with a model ID: the next prompts use it, with the same provider.
    fn set_model(&mut self, next: String) -> Result<Flow> {
        self.built.session.set_model(ModelRef {
            provider: self.built.provider.clone(),
            model: next.clone(),
        })?;
        self.model = next;
        Ok(self.say(Tone::Dim, &format!("[chat] model={}", visible(&self.model))))
    }

    /// /reasoning with a level, or a warning when it names none.
    fn set_reasoning(&mut self, next: &str) -> Result<Flow> {
        match parse_level(next) {
            Ok(level) => {
                self.built.session.set_reasoning(level)?;
                self.reasoning = level;
                Ok(self.say(
                    Tone::Dim,
                    &format!("[chat] reasoning={}", reasoning_name(self.reasoning)),
                ))
            }
            Err(message) => Ok(self.say(Tone::Warn, &format!("[chat] {}", visible(&message)))),
        }
    }

    /// Do what `flow` asks of the REPL. Returns the exit code when the chat ends.
    async fn follow(&mut self, flow: Flow, terminal: &mut Terminal) -> Option<u8> {
        match flow {
            Flow::Continue => {}
            Flow::Prompt(text) => terminal.next_prompt = Some(text),
            Flow::Exit(code) => return Some(code),
            Flow::New => self.new_conversation().await,
        }
        None
    }

    /// /new: a new conversation where conversations are kept.
    async fn new_conversation(&mut self) {
        let sibling;
        let file = match &self.storage {
            Storage::Memory => SessionFile::Memory,
            Storage::File(original) => {
                sibling = new_file(original);
                SessionFile::Create(&sibling)
            }
            Storage::Directory(dir) => SessionFile::NewIn(dir),
        };
        if self
            .chat
            .switch(&mut self.built, &self.model, file, "create new conversation")
            .await
        {
            let file = self
                .built
                .about
                .session
                .as_deref()
                .map(|path| format!("; session={}", path.display()))
                .unwrap_or_default();
            self.chat
                .output
                .line(Tone::Dim, &format!("[chat] new conversation{file}"));
        }
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
