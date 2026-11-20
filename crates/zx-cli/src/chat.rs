//! `zx-agent chat`: a conversation in the terminal, line by line; model and tool work stays in
//! Session.
//!
//! - `input`: the single owner of the terminal's input
//! - `turn`: one operation of a conversation driven to its end, with the approvals its
//!   calls wait for
//! - `output`: what a conversation shows as it runs, streamed
//! - `term`: the lines drawn on the terminal, and their colors
//! - `commands`: slash commands, which never reach the model
//! - `storage`: where conversations are kept, and which one opens first
//! - `info`: the banner, `/status` and `/help`

mod commands;
mod info;
mod input;
mod output;
mod storage;
mod term;
mod turn;

use std::io::IsTerminal;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use zx_agent::Session;
use zx_agent::ai::{Provider, Reasoning};
use zx_agent::input::Input;
use zx_agent::session::SessionError;
use zx_app::config::Builtin;
use zx_app::otel::{Telemetry, TraceHook};
use zx_app::shutdown::{Interrupt, Signals};
use zx_app::{Application, Approvals, SessionPlace};

use crate::application::{connect, open_or_create};
use crate::cli::{self, ChatArgs};
use crate::process::{EXIT_CANCELLED, EXIT_COMPLETED, EXIT_ERROR, with_telemetry};
use crate::render::visible;

use self::commands::Command;
use self::info::{About, banner, help, status};
use self::input::Line;
use self::output::Output;
use self::storage::{Keeping, Storage, opening};
use self::term::Tone;
use self::turn::{Operation, TurnIo, drive_turn};

/// A second Ctrl+C within this long, while idle, exits.
const QUIT_INTERRUPT_WINDOW: Duration = Duration::from_secs(2);
/// The answers to an approval request, as the request and /help show them.
const APPROVAL_MENU: &str = "1/y allow once · 2 allow identical calls in this conversation · 3/n deny · 4 discuss";
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
    session: Session,
    /// Where the session is kept, to open it again from there.
    place: SessionPlace,
    provider: Arc<dyn Provider>,
    about: About,
    /// The commands extensions registered: name and description, for parsing and /help.
    commands: Vec<(String, String)>,
}

/// What every conversation of this chat is built with.
struct Chat {
    app: Application,
    output: Output,
}

impl Chat {
    /// A conversation of `model` kept in `place`: the agent is built for it, with the agent
    /// file read again, and the conversation is its one session, the session `resume` names
    /// or a new one.
    async fn build(&self, model: &str, place: &SessionPlace, resume: Option<&str>) -> Result<Built> {
        // The person at the terminal answers; `drive_turn` passes the answer on.
        let approvals = Approvals::Person("shell commands require interactive approval");
        let agent = self.app.build(model, place, approvals)?;
        let session = open_or_create(&agent, resume).await?;
        let about = About::of(&agent, &session, place.path_of(session.id()).await?)?;
        let commands = session
            .commands()
            .iter()
            .map(|command| (command.name().to_string(), command.description().to_string()))
            .collect();
        Ok(Built {
            session,
            place: place.clone(),
            provider: Arc::clone(agent.provider()),
            about,
            commands,
        })
    }
}

pub(crate) async fn run(args: ChatArgs) -> Result<u8> {
    with_telemetry(async move |telemetry| converse(args, telemetry.map(Telemetry::hook)).await).await
}

async fn converse(args: ChatArgs, traces: Option<TraceHook>) -> Result<u8> {
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
    let app = connect(cli::overrides(base, &budget), Builtin::Assistant)
        .await?
        .set_traces(traces);
    let mode = if std::io::stdin().is_terminal() {
        InputMode::Terminal
    } else {
        InputMode::Script
    };
    let chat = Chat {
        output: Output::new(output_mode, deltas, color),
        app,
    };
    let output = &chat.output;
    let model = chat.app.model().to_string();
    let keeping = Keeping::of(no_session, session, resume.as_ref());
    let (storage, first) = opening(keeping, chat.app.cwd()).await?;
    let built = chat.build(&model, &storage.place, first.id()).await?;
    let mut terminal = TerminalInput {
        signals: Signals::new().context("cannot register chat signal handlers")?,
        input: input::spawn(mode)?,
        next_approval: 1,
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
        reasoning: chat.app.reasoning(),
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
        let flow = match repl.command(command, &mut terminal).await {
            Ok(flow) => flow,
            Err(error) if closed(&error) => repl.reopen(&error).await,
            Err(error) => return Err(error),
        };
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

/// The input side of the REPL: the terminal's lines and signals, the number of the next
/// approval request, and what the next read starts from.
struct TerminalInput {
    input: input::Lines,
    /// Approval requests are numbered through the chat, so that a number names one request.
    next_approval: u64,
    signals: Signals,
    mode: InputMode,
    /// A prompt to send next without reading, such as input that arrived as a task ended.
    next_prompt: Option<String>,
    /// When Ctrl+C was last pressed while idle; a second one soon after exits.
    last_interrupt: Option<Instant>,
}

impl TerminalInput {
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
                Interrupt::Terminate => Read::Exit(EXIT_CANCELLED),
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
            next_approval: &mut self.next_approval,
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
    async fn command(&mut self, command: Command, terminal: &mut TerminalInput) -> Result<Flow> {
        let output = &self.chat.output;
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
    async fn operate(&self, operation: Operation, terminal: &mut TerminalInput) -> Result<Flow> {
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
        self.built
            .session
            .set_model(Arc::clone(&self.built.provider), next.clone())?;
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
    async fn follow(&mut self, flow: Flow, terminal: &mut TerminalInput) -> Option<u8> {
        match flow {
            Flow::Continue => {}
            Flow::Prompt(text) => terminal.next_prompt = Some(text),
            Flow::Exit(code) => return Some(code),
            Flow::New => self.new_conversation().await,
        }
        None
    }

    /// /new: a new conversation where conversations are kept. When it cannot be made, the
    /// current one stays.
    async fn new_conversation(&mut self) {
        let beside = self.storage.beside();
        let place = beside.as_ref().unwrap_or(&self.storage.place);
        let output = &self.chat.output;
        match self.chat.build(&self.model, place, None).await {
            Ok(next) => {
                self.built = next;
                output.reset_totals();
                self.keep_reasoning();
                let file = self
                    .built
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
                    "[chat] cannot create a new conversation: {}",
                    visible(&format!("{error:#}"))
                ),
            ),
        }
    }

    /// The conversation's session closed itself, as it does once its record can no longer be
    /// written: say why, and open it again from where it is kept. A session kept in memory
    /// is forgotten once closed, so the chat ends then, and when opening it again fails.
    async fn reopen(&mut self, error: &anyhow::Error) -> Flow {
        let output = &self.chat.output;
        output.line(Tone::Error, &format!("[chat] {}", visible(&format!("{error:#}"))));
        if !self.built.place.is_persistent() {
            return Flow::Exit(EXIT_ERROR);
        }
        let id = self.built.session.id().to_string();
        let place = self.built.place.clone();
        match self.chat.build(&self.model, &place, Some(&id)).await {
            Ok(next) => {
                self.built = next;
                self.keep_reasoning();
                output.line(Tone::Dim, "[chat] session opened again from where it is kept");
                Flow::Continue
            }
            Err(reopen) => {
                output.line(
                    Tone::Error,
                    &format!(
                        "[chat] cannot open the session again: {}",
                        visible(&format!("{reopen:#}"))
                    ),
                );
                Flow::Exit(EXIT_ERROR)
            }
        }
    }

    /// Give a new or reopened session the reasoning level /reasoning chose; it starts with
    /// the configured one.
    fn keep_reasoning(&self) {
        if self.reasoning == self.chat.app.reasoning() {
            return;
        }
        if let Err(error) = self.built.session.set_reasoning(self.reasoning) {
            self.chat.output.line(
                Tone::Error,
                &format!(
                    "[chat] cannot keep reasoning={}: {error}",
                    reasoning_name(self.reasoning)
                ),
            );
        }
    }
}

/// Whether `error` says the conversation's session is closed.
fn closed(error: &anyhow::Error) -> bool {
    error.downcast_ref::<SessionError>() == Some(&SessionError::Closed)
}

/// Run an extension's command and show what it says. Returns the message it sends, if any.
async fn run_extension_command(session: &Session, name: &str, args: &str, output: &Output) -> Option<Input> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closed_session_is_told_from_other_errors_however_it_is_carried() {
        assert!(closed(&anyhow::Error::from(SessionError::Closed)));
        assert!(closed(
            &anyhow::Error::from(SessionError::Closed).context("cannot start the prompt")
        ));
        assert!(!closed(&anyhow::Error::from(SessionError::Busy)));
        assert!(!closed(&anyhow::anyhow!("session is closed")));
    }
}
