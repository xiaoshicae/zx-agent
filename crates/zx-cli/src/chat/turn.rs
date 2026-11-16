//! One operation of a conversation driven to its end: the prompt, continuation or compaction
//! runs while the terminal takes steering input, approvals and signals.
//!
//! The operation is a run that this loop reads: each event is shown as it arrives, and a
//! call that waits for approval is announced by one. The person's answer goes back through
//! the session, with the ID the run gave the request.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::Value;

use zx_agent::compaction;
use zx_agent::events::{ApprovalResult, Event, EventKind, RunReport};
use zx_agent::input::{Input, PendingInput};
use zx_agent::policy::ApprovalDecision;
use zx_agent::session::SessionError;
use zx_agent::{Run, Session};

use super::commands::{self, Command};
use super::info::help;
use super::input::{self, Line};
use super::output::Output;
use super::term::Tone;
use super::{Flow, InputMode, rejected, run_extension_command};
use crate::process::{EXIT_CANCELLED, EXIT_COMPLETED};
use crate::process::{Interrupt, Signals};
use crate::render::{describe_arguments, describe_call, visible};

const APPROVAL_MENU: &str = "  1/y allow once · 2 allow identical calls in this conversation · 3/n deny · 4 discuss";
const APPROVAL_PROMPT: &str = "[approval] Choose 1, 2, 3 or 4:";

pub(super) struct TurnIo<'a> {
    pub(super) input: &'a mut input::Input,
    /// The number the next approval request is shown under; it counts through the chat.
    pub(super) next_approval: &'a mut u64,
    pub(super) signals: &'a mut Signals,
    pub(super) mode: InputMode,
    pub(super) output: &'a Output,
    /// The commands extensions registered, for lines typed during the task.
    pub(super) commands: &'a [(String, String)],
}

pub(super) enum Operation {
    Prompt(String),
    /// A message an extension's command sends, as a prompt.
    Message(Input),
    Continue,
    Compact,
}

/// The run of an operation: a task, or a compaction.
enum Running {
    Task(Run),
    Compact(Run<compaction::Report>),
}

impl Running {
    /// Start `operation` on `session`.
    fn start(session: &Session, operation: Operation) -> Result<Self, SessionError> {
        Ok(match operation {
            Operation::Prompt(text) => Self::Task(session.send(text)?),
            Operation::Message(message) => Self::Task(session.send(message)?),
            Operation::Continue => Self::Task(session.continue_run()?),
            Operation::Compact => Self::Compact(session.compact()?),
        })
    }

    /// The run's next event; `None` once it has ended.
    async fn next(&mut self) -> Option<Event> {
        match self {
            Self::Task(run) => run.next().await,
            Self::Compact(run) => run.next().await,
        }
    }

    /// Read the run to its end, showing the events that remain, and return its report.
    async fn report(mut self, output: &Output) -> Report {
        while let Some(event) = self.next().await {
            output.emit(&event);
        }
        match self {
            Self::Task(run) => Report::Task(run.wait().await),
            Self::Compact(run) => Report::Compact(run.wait().await),
        }
    }
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

/// A call that waits for the person's approval.
struct Ticket {
    /// The number the request is shown under.
    number: u64,
    /// The request's ID in the run, which its answer names.
    approval_id: String,
    tool: String,
    arguments: Value,
    reason: String,
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

    fn ticket(&self) -> Option<&Ticket> {
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
pub(super) async fn drive_turn(session: &Session, operation: Operation, io: &mut TurnIo<'_>) -> Result<Flow> {
    let mut state = TurnState {
        compact: matches!(operation, Operation::Compact),
        started: Instant::now(),
        pending: Pending::None,
        queued_prompt: None,
    };
    let mut run = Running::start(session, operation)?;
    loop {
        tokio::select! {
            biased;
            interrupt = io.signals.next() => return interrupted(session, run, io, &mut state, interrupt).await,
            event = run.next() => match event {
                Some(event) => observed(io, &mut state.pending, &event),
                None => return finished(session, &run.report(io.output).await, io, state),
            },
            line = io.input.recv(), if io.mode == InputMode::Terminal || state.pending.waiting() => {
                if let Some(next) = typed(session, io, &mut state, line).await? {
                    drop(state.pending.take());
                    settle(session, run, io.output, state.started).await;
                    return Ok(next);
                }
            }
        }
    }
}

/// An event of the operation: show it, and follow the approval it announces or ends.
fn observed(io: &mut TurnIo<'_>, pending: &mut Pending, event: &Event) {
    io.output.emit(event);
    match &event.event {
        EventKind::ApprovalRequested {
            approval_id,
            tool_name,
            args,
            reason,
            ..
        } => {
            let number = *io.next_approval;
            *io.next_approval = number.saturating_add(1);
            let ticket = Ticket {
                number,
                approval_id: approval_id.clone(),
                tool: tool_name.clone(),
                arguments: args.clone(),
                reason: reason.clone(),
            };
            show_approval(io.output, &ticket);
            *pending = Pending::Choosing(ticket);
        }
        EventKind::ApprovalResolved {
            approval_id: Some(ended),
            approval: ApprovalResult::TimedOut | ApprovalResult::Cancelled,
            ..
        } => {
            // The run stopped waiting before the person answered.
            if pending.ticket().is_some_and(|ticket| &ticket.approval_id == ended)
                && let Some(ticket) = pending.take()
            {
                io.output.line(
                    Tone::Warn,
                    &format!(
                        "[approval] request {} expired; the task goes on without the call",
                        ticket.number
                    ),
                );
            }
        }
        _ => {}
    }
}

/// Ctrl+C or SIGTERM during the operation: cancel it, report how far it got, and drop the
/// input meant for it.
async fn interrupted(
    session: &Session,
    run: Running,
    io: &TurnIo<'_>,
    state: &mut TurnState,
    interrupt: Interrupt,
) -> Result<Flow> {
    drop(state.pending.take());
    settle(session, run, io.output, state.started).await;
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
        Interrupt::Terminate => Flow::Exit(EXIT_CANCELLED),
    })
}

/// The operation ended: print its report, and turn the input that arrived too late for it
/// into the next prompt.
fn finished(session: &Session, report: &Report, io: &TurnIo<'_>, state: TurnState) -> Result<Flow> {
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
            Input::User { text, .. } => next.push(text),
            custom @ Input::Custom(_) => session.queues().steer(custom),
        }
    }
    Ok(if next.is_empty() {
        Flow::Continue
    } else {
        Flow::Prompt(next.join("\n\n"))
    })
}

/// A line typed during the operation: an answer to the approval menu, steering, or a
/// command. Returns what the REPL does next when the line ends the operation, which the
/// caller then cancels.
async fn typed(
    session: &Session,
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
                deny(session, io.output, &ticket, "terminal input closed");
            }
            return Ok(Some(Flow::Exit(EXIT_COMPLETED)));
        }
    };
    match command {
        Command::New => Ok(Some(Flow::New)),
        Command::Quit => Ok(Some(Flow::Exit(EXIT_COMPLETED))),
        command => {
            busy(session, io, state, command).await;
            Ok(None)
        }
    }
}

/// A command typed while the operation runs: steering and the commands that do not touch the
/// session run now; the others wait for the operation to end.
async fn busy(session: &Session, io: &TurnIo<'_>, state: &mut TurnState, command: Command) {
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
        Command::Help => help(output, io.commands),
        // Extension commands run at once, as in pi; a message they send waits for the next
        // request as steering input.
        Command::Extension { name, args } => {
            if let Some(message) = run_extension_command(session, &name, &args, output).await {
                session.queues().steer(message);
                output.line(Tone::Dim, "[chat] the command's message is queued as steering input");
            }
        }
        Command::Compact => output.line(
            Tone::Warn,
            "[chat] session is busy; wait for the current operation or cancel it before /compact",
        ),
        Command::Invalid(message) => output.line(Tone::Warn, &format!("[chat] {}", visible(&message))),
        Command::Empty => {}
    }
}

/// Cancel the operation, read it to its end and report how far it got.
async fn settle(session: &Session, run: Running, output: &Output, started: Instant) {
    session.cancel();
    run.report(output).await.print(output, started.elapsed());
}

/// A line typed while a call waits for approval: a menu choice, or the message that
/// discusses the call instead of running it.
fn answer_approval(session: &Session, output: &Output, pending: &mut Pending, text: String) {
    match std::mem::replace(pending, Pending::None) {
        Pending::None => {}
        Pending::Discussing(ticket) => {
            if text.trim().is_empty() {
                output.line(Tone::Warn, "[approval] enter a message to discuss this call");
                *pending = Pending::Discussing(ticket);
            } else {
                session.queues().steer(text);
                deny(session, output, &ticket, "user wants to discuss this call");
            }
        }
        Pending::Choosing(ticket) => match approval_choice(&text) {
            Some(Choice::Once) => answer(session, output, &ticket, ApprovalDecision::AllowOnce),
            Some(Choice::Session) => answer(session, output, &ticket, ApprovalDecision::AllowSession),
            Some(Choice::Deny) => deny(session, output, &ticket, "declined by the user"),
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

/// Give the person's decision to the run; it may have stopped waiting for it.
fn answer(session: &Session, output: &Output, ticket: &Ticket, decision: ApprovalDecision) {
    if session.resolve_approval(&ticket.approval_id, decision).is_err() {
        // No call was authorized: the request ended before the answer came.
        output.line(Tone::Warn, &format!("[approval] request {} expired", ticket.number));
    }
}

fn deny(session: &Session, output: &Output, ticket: &Ticket, reason: &str) {
    answer(session, output, ticket, ApprovalDecision::Deny(reason.into()));
}

/// Input that `steer` queued and no run read; a task only reads it between turns.
fn unread_input(session: &Session, output: &Output) -> Vec<Input> {
    match session.take_queued() {
        Ok(queued) => queued.into_iter().map(PendingInput::into_input).collect(),
        Err(error) => {
            output.line(
                Tone::Error,
                &format!("[chat] cannot take the unread input: {}", visible(&error.to_string())),
            );
            Vec::new()
        }
    }
}

/// A queued message in one line, for notices.
fn describe_queued(message: &Input) -> String {
    match message {
        Input::User { text, .. } => text.clone(),
        Input::Custom(custom) => format!("[{}] {}", custom.custom_type, custom.content),
    }
}

fn show_approval(output: &Output, ticket: &Ticket) {
    let style = output.style();
    // The label is colored, the call itself is bold in the default color: it is what a
    // person has to read.
    output.line(
        Tone::Plain,
        &format!(
            "{} {}",
            style.paint(Tone::Warn, &format!("[approval {}]", ticket.number)),
            style.paint(Tone::Strong, &describe_call(&ticket.tool, &ticket.arguments))
        ),
    );
    for line in describe_arguments(&ticket.arguments) {
        output.line(Tone::Plain, &format!("  {line}"));
    }
    output.line(Tone::Dim, &format!("  {}", visible(&ticket.reason)));
    output.line(Tone::Dim, APPROVAL_MENU);
    output.line(Tone::Warn, APPROVAL_PROMPT);
}
