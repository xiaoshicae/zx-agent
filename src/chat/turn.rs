//! One operation of a conversation driven to its end: the prompt, continuation or summary
//! runs while the terminal takes steering input, approvals and signals.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

use zx_agent::AgentSession;
use zx_agent::agent::Approval;
use zx_agent::agent::QueuedMessage;
use zx_agent::agent_session::AgentSessionError;
use zx_agent::compaction;
use zx_agent::runner::RunReport;

use super::commands::{self, Command};
use super::info::help;
use super::input::{self, Line};
use super::interaction::Ticket;
use super::output::Output;
use super::signals::{Interrupt, Signals};
use super::term::Tone;
use super::{Flow, InputMode, queue_line, rejected, run_extension_command, set_queue_mode};
use crate::process::{EXIT_ABORTED, EXIT_COMPLETED};
use crate::render::{describe_arguments, describe_call, visible};

/// How often the status line is redrawn while a task runs.
const TICK: Duration = Duration::from_secs(1);
const APPROVAL_MENU: &str = "  1/y allow once · 2 allow identical calls in this conversation · 3/n deny · 4 discuss";
const APPROVAL_PROMPT: &str = "[approval] Choose 1, 2, 3 or 4:";

pub(super) struct TurnIo<'a> {
    pub(super) input: &'a mut input::Input,
    pub(super) approvals: &'a mut mpsc::Receiver<Ticket>,
    pub(super) signals: &'a mut Signals,
    pub(super) mode: InputMode,
    pub(super) output: &'a Output,
    /// The commands extensions registered, for lines typed during the task.
    pub(super) commands: &'a [(String, String)],
}

pub(super) enum Operation {
    Prompt(String),
    /// A message an extension's command sends, as a prompt.
    Message(QueuedMessage),
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

/// The approval a call waits for, while the person decides.
enum Pending {
    None,
    /// The menu is shown; the next line answers it.
    Choosing(Ticket),
    /// The person chose to discuss: the next line goes to the model, and the call is denied.
    Discussing(Ticket),
}

impl Pending {
    fn waiting(&self) -> bool {
        !matches!(self, Self::None)
    }

    fn ticket_mut(&mut self) -> Option<&mut Ticket> {
        match self {
            Self::None => None,
            Self::Choosing(ticket) | Self::Discussing(ticket) => Some(ticket),
        }
    }

    fn take(&mut self) -> Option<Ticket> {
        match std::mem::replace(self, Self::None) {
            Self::None => None,
            Self::Choosing(ticket) | Self::Discussing(ticket) => Some(ticket),
        }
    }
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

/// What one operation keeps while it runs: the approval a call waits for, the input typed
/// during a compaction, and when it started.
struct TurnState {
    compact: bool,
    started: Instant,
    pending: Pending,
    queued_prompt: Option<String>,
}

/// Run `operation` on `session` while the terminal takes steering input, approvals and
/// signals. Returns what the REPL does next.
pub(super) async fn drive_turn(session: &AgentSession, operation: Operation, io: &mut TurnIo<'_>) -> Result<Flow> {
    let mut state = TurnState {
        compact: matches!(operation, Operation::Compact),
        started: Instant::now(),
        pending: Pending::None,
        queued_prompt: None,
    };
    // `settle` cancels the operation through the session; nothing cancels this token.
    let cancel = CancellationToken::new();
    let running = async {
        match operation {
            Operation::Prompt(text) => session.prompt(text, &cancel).await.map(Report::Task),
            Operation::Message(message) => session.prompt(message, &cancel).await.map(Report::Task),
            Operation::Continue => session.continue_run(&cancel).await.map(Report::Task),
            Operation::Compact => session.compact(&cancel).await.map(Report::Compact),
        }
    };
    tokio::pin!(running);
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // A status an earlier operation left behind must not tick on into this one.
    io.output.clear_status();
    loop {
        if state
            .pending
            .ticket_mut()
            .is_some_and(|ticket| ticket.reply.is_closed())
        {
            state.pending = Pending::None;
        }
        tokio::select! {
            biased;
            interrupt = io.signals.next() => return interrupted(session, running.as_mut(), io, &mut state, interrupt).await,
            report = &mut running => return finished(session, &report?, io, state),
            () = expired(&mut state.pending), if state.pending.waiting() => {
                if let Some(ticket) = state.pending.take() {
                    io.output.line(Tone::Warn, &format!("[approval] request {} expired; the task goes on without the call", ticket.id));
                }
            }
            request = io.approvals.recv() => {
                let Some(ticket) = request else { anyhow::bail!("terminal approval channel closed"); };
                asked(io.output, &mut state.pending, ticket);
            }
            line = io.input.recv(), if io.mode == InputMode::Terminal || state.pending.waiting() => {
                if let Some(flow) = typed(session, running.as_mut(), io, &mut state, line).await? {
                    return Ok(flow);
                }
            }
            _ = tick.tick() => io.output.tick(),
        }
    }
}

/// Ctrl+C or SIGTERM during the operation: cancel it, report how far it got, and drop the
/// input meant for it.
async fn interrupted(
    session: &AgentSession,
    running: Pin<&mut impl Future<Output = Result<Report, AgentSessionError>>>,
    io: &TurnIo<'_>,
    state: &mut TurnState,
    interrupt: Interrupt,
) -> Result<Flow> {
    drop(state.pending.take());
    settle(session, running, io.output, state.started).await?;
    if state.queued_prompt.is_some() {
        io.output.line(Tone::Dim, "[chat] queued input cleared by cancellation");
    }
    // Steering meant for the cancelled task must not reach the next one.
    let dropped = unread_input(session, io.output);
    if !dropped.is_empty() {
        let described: Vec<String> = dropped.iter().map(describe_queued).collect();
        io.output.line(
            Tone::Dim,
            &format!(
                "[chat] cancellation discarded {} steering input(s): {}",
                dropped.len(),
                visible(&described.join(" / "))
            ),
        );
    }
    Ok(match interrupt {
        Interrupt::Cancel => Flow::Continue,
        Interrupt::Terminate => Flow::Exit(EXIT_ABORTED),
    })
}

/// The operation ended: print its report, and turn the input that arrived too late for it
/// into the next prompt.
fn finished(session: &AgentSession, report: &Report, io: &TurnIo<'_>, state: TurnState) -> Result<Flow> {
    report.print(io.output, state.started.elapsed());
    // Steering that arrived as the task ended was never read: ask it next.
    let late = unread_input(session, io.output);
    if !late.is_empty() {
        io.output.line(
            Tone::Dim,
            "[chat] steering input arrived as the task ended; sending it as the next prompt",
        );
    }
    // The person's text becomes the next prompt; a message an extension queued goes back to
    // the queue, which the next run takes before its first request.
    let mut next: Vec<String> = state.queued_prompt.into_iter().collect();
    for message in late {
        match message {
            QueuedMessage::User(text) => next.push(text),
            custom @ QueuedMessage::Custom(_) => session.queues().steer_message(custom),
        }
    }
    Ok(if next.is_empty() {
        Flow::Continue
    } else {
        Flow::Prompt(next.join("\n\n"))
    })
}

/// A call asks for approval: show the menu, and deny a request still open.
fn asked(output: &Output, pending: &mut Pending, ticket: Ticket) {
    if ticket.reply.is_closed() {
        return;
    }
    if let Some(previous) = pending.take() {
        // Defensive fail-closed behavior; the core normally asks serially.
        deny(output, previous, "superseded terminal approval");
    }
    show_approval(output, &ticket);
    *pending = Pending::Choosing(ticket);
}

/// A line typed during the operation: an answer to the approval menu, steering, or a
/// command. Returns what the REPL does next when the line ends the operation.
async fn typed(
    session: &AgentSession,
    running: Pin<&mut impl Future<Output = Result<Report, AgentSessionError>>>,
    io: &mut TurnIo<'_>,
    state: &mut TurnState,
    line: Option<std::io::Result<Line>>,
) -> Result<Option<Flow>> {
    if io.mode == InputMode::Terminal && line.is_some() {
        io.output.input_echoed();
    }
    let command = match line {
        Some(line) => match line.context("cannot read terminal input")? {
            Line::Text(text) => {
                if state.pending.waiting() && text.trim() != "/quit" && text.trim() != "/new" {
                    answer_approval(session, io.output, &mut state.pending, text);
                    return Ok(None);
                }
                commands::parse(text, io.commands)
            }
            Line::Rejected(reason) => {
                rejected(io.output, &reason);
                return Ok(None);
            }
        },
        None => {
            if let Some(ticket) = state.pending.take() {
                deny(io.output, ticket, "terminal input closed");
            }
            settle(session, running, io.output, state.started).await?;
            return Ok(Some(Flow::Exit(EXIT_COMPLETED)));
        }
    };
    if matches!(command, Command::Quit | Command::New) {
        let next = if matches!(command, Command::New) {
            Flow::New
        } else {
            Flow::Exit(EXIT_COMPLETED)
        };
        drop(state.pending.take());
        settle(session, running, io.output, state.started).await?;
        return Ok(Some(next));
    }
    busy(session, io, state, command).await;
    Ok(None)
}

/// A command typed while the operation runs: steering and the commands that do not touch the
/// session run now; the others wait for the operation to end.
async fn busy(session: &AgentSession, io: &TurnIo<'_>, state: &mut TurnState, command: Command) {
    let output = io.output;
    match command {
        Command::Prompt(text) if state.compact => {
            if state.queued_prompt.is_none() {
                state.queued_prompt = Some(text);
                output.line(Tone::Dim, "[chat] input queued after compaction");
            } else {
                output.line(
                    Tone::Warn,
                    "[chat] one input is already queued; wait for compaction or cancel it",
                );
            }
        }
        Command::Prompt(text) if !state.pending.waiting() => {
            session.queues().steer(text);
            output.line(Tone::Dim, "[chat] steering input queued");
        }
        Command::Prompt(_) => unreachable!("pending approval input is handled above"),
        Command::Quit | Command::New => unreachable!("quit and new end the operation above"),
        Command::Model(_) => output.line(
            Tone::Warn,
            "[chat] wait for the current task or cancel it before changing models",
        ),
        Command::Reasoning(_) => output.line(
            Tone::Warn,
            "[chat] wait for the current task or cancel it before changing the reasoning level",
        ),
        Command::Continue => output.line(
            Tone::Warn,
            "[chat] session is busy; wait for the current operation or cancel it before /continue",
        ),
        Command::Status => output.line(
            Tone::Warn,
            "[chat] wait for the current task or cancel it before /status",
        ),
        Command::Sessions => output.line(
            Tone::Warn,
            "[chat] wait for the current task or cancel it before /sessions",
        ),
        Command::Resume(_) => output.line(
            Tone::Warn,
            "[chat] wait for the current task or cancel it before /resume",
        ),
        Command::Fork => output.line(Tone::Warn, "[chat] wait for the current task or cancel it before /fork"),
        Command::Tree => output.line(Tone::Warn, "[chat] wait for the current task or cancel it before /tree"),
        Command::Branch(_) => output.line(
            Tone::Warn,
            "[chat] wait for the current task or cancel it before /branch",
        ),
        Command::Help => help(output, io.commands),
        // Extension commands run at once, as in pi; a message they send waits for the next
        // request as steering input.
        Command::Extension { name, args } => {
            if let Some(message) = run_extension_command(session, &name, &args, output).await {
                session.queues().steer_message(message);
                output.line(Tone::Dim, "[chat] the command's message is queued as steering input");
            }
        }
        Command::Queue(None) => output.line(Tone::Dim, &queue_line(session)),
        Command::Queue(Some(mode)) => set_queue_mode(session, &mode, output),
        Command::Compact => output.line(
            Tone::Warn,
            "[chat] session is busy; wait for the current operation or cancel it before /compact",
        ),
        Command::Invalid(message) => output.line(Tone::Warn, &format!("[chat] {}", visible(&message))),
        Command::Empty => {}
    }
}

/// Cancel the operation and report how far it got.
async fn settle(
    session: &AgentSession,
    running: Pin<&mut impl Future<Output = Result<Report, AgentSessionError>>>,
    output: &Output,
    started: Instant,
) -> Result<()> {
    session.abort();
    running.await?.print(output, started.elapsed());
    Ok(())
}

/// Completes once the core stops waiting for the pending approval: it timed out, or the
/// task was cancelled. Never completes without one.
async fn expired(pending: &mut Pending) {
    match pending.ticket_mut() {
        Some(ticket) => ticket.reply.closed().await,
        None => std::future::pending().await,
    }
}

/// A line typed while a call waits for approval: a menu choice, or the message that
/// discusses the call instead of running it.
fn answer_approval(session: &AgentSession, output: &Output, pending: &mut Pending, text: String) {
    match std::mem::replace(pending, Pending::None) {
        Pending::None => {}
        Pending::Discussing(ticket) => {
            if text.trim().is_empty() {
                output.line(Tone::Warn, "[approval] enter a message to discuss this call");
                *pending = Pending::Discussing(ticket);
            } else {
                session.queues().steer(text);
                deny(output, ticket, "user wants to discuss this call");
            }
        }
        Pending::Choosing(ticket) => match approval_choice(&text) {
            Some(Choice::Once) => answer(output, ticket, Approval::AllowOnce),
            Some(Choice::Session) => answer(output, ticket, Approval::AllowSession),
            Some(Choice::Deny) => deny(output, ticket, "declined by the user"),
            Some(Choice::Discuss) => {
                output.line(
                    Tone::Warn,
                    "[approval] What would you like to discuss? The call will not run.",
                );
                *pending = Pending::Discussing(ticket);
            }
            None => {
                output.line(Tone::Warn, "[approval] choose 1, 2, 3 or 4");
                *pending = Pending::Choosing(ticket);
            }
        },
    }
}

/// Send the person's decision; the core may have stopped waiting for it.
fn answer(output: &Output, ticket: Ticket, decision: Approval) {
    if ticket.reply.send(decision).is_err() {
        output.line(Tone::Warn, "[approval] request expired");
    }
}

/// Input that `steer` queued and no run read; a task only reads it between turns.
fn unread_input(session: &AgentSession, output: &Output) -> Vec<QueuedMessage> {
    session.take_queued().unwrap_or_else(|error| {
        output.line(
            Tone::Error,
            &format!("[chat] cannot take the unread input: {}", visible(&error.to_string())),
        );
        Vec::new()
    })
}

/// A queued message in one line, for notices.
fn describe_queued(message: &QueuedMessage) -> String {
    match message {
        QueuedMessage::User(text) => text.clone(),
        QueuedMessage::Custom(custom) => format!("[{}] {}", custom.custom_type, custom.content),
    }
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
            style.paint(Tone::Strong, &describe_call(&call.name, &call.arguments))
        ),
    );
    for line in describe_arguments(&call.arguments) {
        output.line(Tone::Plain, &format!("  {line}"));
    }
    output.line(Tone::Dim, &format!("  {}", visible(&ticket.request.reason)));
    output.line(Tone::Dim, APPROVAL_MENU);
    output.line(Tone::Warn, APPROVAL_PROMPT);
}

fn deny(output: &Output, ticket: Ticket, reason: &str) {
    if ticket.reply.send(Approval::Deny(reason.into())).is_err() {
        // The runtime already stopped waiting; no call is authorized.
        output.line(Tone::Warn, &format!("[approval] request {} expired", ticket.id));
    }
}
