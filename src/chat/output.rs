//! Streaming terminal output. Model text is rendered without executable control codes.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;
use zx_agent::agent::{AgentEvent, ApprovalResult, Event, EventSink};
use zx_agent::ai::{ContentBlock, Delta, Message, StopReason, ToolResultMessage};
use zx_agent::compaction::{self, Outcome};
use zx_agent::runner::RunReport;
use zx_agent::sync::lock;
use zx_agent::text::clip;

use crate::cli::OutputMode;
use crate::{JsonlEvents, write_line};

use super::debug::DebugState;
use super::term::{Phase, Style, Terminal, Tone, elapsed_text};

/// Lines of a tool result shown under its `[tool]` line.
const PREVIEW_LINES: usize = 3;
/// Characters kept of each previewed line.
const PREVIEW_CHARS: usize = 120;
/// Characters of a call's label in progress and result lines; approvals show everything.
const LABEL_CHARS: usize = 80;
/// Argument fields that say what a call does, shown instead of the whole JSON.
const LABEL_FIELDS: [&str; 2] = ["command", "path"];

/// What the conversation has used so far, for `/status`.
#[derive(Clone, Copy, Default)]
pub(super) struct Totals {
    pub(super) turns: u64,
    pub(super) requests: u64,
    pub(super) input_tokens: u64,
    pub(super) output_tokens: u64,
    pub(super) cached_tokens: u64,
}

struct TextState {
    terminal: Terminal,
    /// The tool calls of the latest answer by call ID, labelled for their result lines.
    labels: HashMap<String, String>,
    wrote_text: bool,
    totals: Totals,
}

pub(super) struct Output {
    mode: OutputMode,
    debug: bool,
    debug_state: Mutex<DebugState>,
    json: JsonlEvents,
    text: Mutex<TextState>,
}

impl Output {
    pub(super) fn new(mode: OutputMode, deltas: bool, debug: bool) -> Self {
        let stderr_is_terminal = std::io::stderr().is_terminal();
        let style = Style::detect(stderr_is_terminal);
        // A status line is redrawn in place, which a dumb terminal or a log cannot show; the
        // debug timeline prints every step itself.
        let dumb = std::env::var_os("TERM").is_some_and(|value| value == "dumb");
        let live = stderr_is_terminal && !dumb && !debug && mode == OutputMode::Text;
        let spaced = std::io::stdout().is_terminal();
        Self {
            mode,
            debug,
            debug_state: Mutex::new(DebugState::default()),
            json: JsonlEvents { deltas },
            text: Mutex::new(TextState {
                terminal: Terminal::new(style, live, spaced),
                labels: HashMap::new(),
                wrote_text: false,
                totals: Totals::default(),
            }),
        }
    }

    /// One line of housekeeping on stderr.
    pub(super) fn line(&self, tone: Tone, text: &str) {
        lock(&self.text).terminal.line(tone, text);
    }

    pub(super) fn prompt(&self, text: &str) {
        lock(&self.text).terminal.prompt(text);
    }

    /// Text shown after the prompt as if the user had typed it.
    pub(super) fn echo(&self, text: &str) {
        lock(&self.text).terminal.echo(text);
    }

    /// The terminal echoed a line the user typed.
    pub(super) fn input_echoed(&self) {
        lock(&self.text).terminal.input_echoed();
    }

    /// End an open line before the process exits.
    pub(super) fn finish(&self) {
        lock(&self.text).terminal.finish();
    }

    /// Redraw the status line; nothing happens while there is none.
    pub(super) fn tick(&self) {
        lock(&self.text).terminal.tick();
    }

    pub(super) fn totals(&self) -> Totals {
        lock(&self.text).totals
    }

    pub(super) fn reset_totals(&self) {
        lock(&self.text).totals = Totals::default();
    }

    pub(super) fn report(&self, report: &RunReport, elapsed: Duration) {
        match self.mode {
            // The `run_end` event, already written, carries the report.
            OutputMode::Jsonl => {}
            OutputMode::Text => {
                let mut state = lock(&self.text);
                let totals = &mut state.totals;
                totals.turns = totals.turns.saturating_add(1);
                totals.requests = totals.requests.saturating_add(u64::from(report.model_requests));
                totals.input_tokens = totals.input_tokens.saturating_add(report.usage.input);
                totals.output_tokens = totals.output_tokens.saturating_add(report.usage.output);
                totals.cached_tokens = totals.cached_tokens.saturating_add(report.usage.cache_read);
                if self.debug {
                    return;
                }
                if let Some(error) = &report.error {
                    state.terminal.line(Tone::Error, &format!("[chat] {}", visible(error)));
                }
                let requests = match report.model_requests {
                    1 => "1 request".to_string(),
                    n => format!("{n} requests"),
                };
                let retries = match report.retries {
                    0 => String::new(),
                    1 => " · 1 retry".to_string(),
                    n => format!(" · {n} retries"),
                };
                let cached = match report.usage.cache_read {
                    0 => String::new(),
                    n => format!(" · {n} cached"),
                };
                state.terminal.line(
                    Tone::Dim,
                    &format!(
                        "[turn] {requests}{retries} · {} in · {} out{cached} · {}",
                        report.usage.input,
                        report.usage.output,
                        elapsed_text(elapsed)
                    ),
                );
            }
        }
    }

    pub(super) fn compact_report(&self, report: &compaction::Report) {
        match self.mode {
            OutputMode::Jsonl => write_line(&serde_json::json!({"type":"compaction_report","report":report})),
            OutputMode::Text => match report.outcome {
                Outcome::Noop => self.line(Tone::Dim, "[compact] no eligible old messages to summarize"),
                Outcome::Completed => self.line(
                    Tone::Dim,
                    &format!(
                        "[compact] requests={} tokens={}",
                        report.requests, report.usage.total_tokens
                    ),
                ),
                Outcome::Failed | Outcome::Aborted => {
                    if let Some(error) = &report.error {
                        self.line(Tone::Error, &format!("[compact] {}", visible(error)));
                    }
                }
            },
        }
    }

    fn tool_result(&self, state: &mut TextState, result: &ToolResultMessage) {
        let label = state
            .labels
            .remove(&result.tool_call_id)
            .unwrap_or_else(|| result.tool_name.clone());
        let status = if result.is_error { "error" } else { "ok" };
        let duration = result.duration_ms.map(|ms| format!(" {ms}ms")).unwrap_or_default();
        let tone = if result.is_error { Tone::Warn } else { Tone::Dim };
        state
            .terminal
            .tool_done(tone, &format!("[tool] {label} -> {status}{duration}"));
        for line in preview(&result.content) {
            state.terminal.line(Tone::Dim, &format!("    {line}"));
        }
    }
}

impl EventSink for Output {
    fn emit(&self, event: &Event) {
        if self.mode == OutputMode::Jsonl {
            self.json.emit(event);
            return;
        }
        {
            let mut state = lock(&self.text);
            match &event.event {
                AgentEvent::RequestStart { .. } => state.terminal.set_phase(Phase::Model { thinking: false }),
                AgentEvent::MessageStart { role: "assistant" } => state.wrote_text = false,
                AgentEvent::MessageUpdate {
                    delta: Delta::Text { text },
                } => {
                    state.terminal.answer_text(&visible(text));
                    state.wrote_text = true;
                }
                AgentEvent::MessageUpdate {
                    delta: Delta::Thinking { .. },
                } => state.terminal.set_phase(Phase::Model { thinking: true }),
                AgentEvent::MessageEnd {
                    message: Message::Assistant(message),
                    ..
                } => {
                    let text = message.text();
                    if !state.wrote_text && !text.is_empty() {
                        state.terminal.answer_text(&visible(&text));
                    }
                    state.terminal.end_answer();
                    for block in &message.content {
                        if let ContentBlock::ToolCall {
                            id, name, arguments, ..
                        } = block
                        {
                            state
                                .labels
                                .insert(id.clone(), clip(&describe_call(name, arguments), LABEL_CHARS));
                        }
                    }
                    if !self.debug {
                        if let Some(error) = &message.error_message {
                            // A cancelled request is reported once, with the run's report.
                            if message.stop_reason != StopReason::Aborted {
                                state.terminal.line(
                                    Tone::Error,
                                    &format!("[model] {:?}: {}", message.stop_reason, visible(error)),
                                );
                            }
                        } else if message.stop_reason == StopReason::Length {
                            state
                                .terminal
                                .line(Tone::Warn, "[model] response reached the output token limit");
                        }
                    }
                }
                AgentEvent::MessageEnd {
                    message: Message::ToolResult(result),
                    ..
                } => {
                    if !self.debug {
                        self.tool_result(&mut state, result);
                    }
                }
                AgentEvent::ToolExecutionStart {
                    tool_call_id,
                    tool_name,
                    args,
                } => {
                    let label = state
                        .labels
                        .get(tool_call_id)
                        .cloned()
                        .unwrap_or_else(|| clip(&describe_call(tool_name, args), LABEL_CHARS));
                    state.terminal.set_phase(Phase::Tool { label });
                }
                // The result line that follows says how the call ended.
                AgentEvent::ToolExecutionEnd { .. } => {}
                AgentEvent::ApprovalRequested { .. } | AgentEvent::RunEnd { .. } => state.terminal.clear_status(),
                // The call starts only now; `tool_execution_start` came before the question.
                AgentEvent::ApprovalResolved {
                    tool_call_id,
                    approval: ApprovalResult::AllowOnce | ApprovalResult::AllowSession | ApprovalResult::Remembered,
                    ..
                } => {
                    if let Some(label) = state.labels.get(tool_call_id).cloned() {
                        state.terminal.set_phase(Phase::Tool { label });
                    }
                }
                // Everything else is printed as `run` prints it; `progress_line` covers every event.
                other => {
                    if !self.debug
                        && let Some(line) = progress_line(other)
                    {
                        state.terminal.line(progress_tone(other), &line);
                    }
                }
            }
        }
        // Emit after rendering the assistant message so the log starts on a new line.
        if self.debug {
            let color = lock(&self.text).terminal.style().color();
            lock(&self.debug_state).emit(event, color);
        }
    }
}

/// `name: <main argument>`, or `name: <arguments as JSON>`; nothing is cut, control
/// characters are escaped and a multi-line command stays on one line.
pub(super) fn describe_call(name: &str, args: &Value) -> String {
    let detail = args
        .as_object()
        .and_then(|fields| {
            LABEL_FIELDS
                .iter()
                .find_map(|field| fields.get(*field).and_then(Value::as_str))
        })
        .map_or_else(|| args.to_string(), str::to_string);
    format!("{}: {}", visible(name), visible(&detail).replace('\n', " "))
}

/// The arguments [`describe_call`] left out, as JSON, when there are any.
pub(super) fn remaining_arguments(args: &Value) -> Option<String> {
    let fields = args.as_object()?;
    let shown = LABEL_FIELDS
        .iter()
        .find(|field| fields.get(**field).is_some_and(Value::is_string))?;
    let rest: serde_json::Map<String, Value> = fields
        .iter()
        .filter(|(key, _)| key.as_str() != *shown)
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    (!rest.is_empty()).then(|| visible(&Value::Object(rest).to_string()))
}

/// The first lines of a tool result, each cut short, and how many more there are.
fn preview(content: &str) -> Vec<String> {
    let total = content.lines().count();
    let mut lines: Vec<String> = content
        .lines()
        .take(PREVIEW_LINES)
        .map(|line| clip(&visible(line), PREVIEW_CHARS))
        .collect();
    if total > PREVIEW_LINES {
        lines.push(format!("… {} more line(s)", total - PREVIEW_LINES));
    }
    lines
}

fn progress_tone(event: &AgentEvent) -> Tone {
    match event {
        AgentEvent::RetryScheduled { .. }
        | AgentEvent::ContextOverflow { .. }
        | AgentEvent::ToolCallRecovered { .. } => Tone::Warn,
        AgentEvent::CompactionFailed { .. } | AgentEvent::HookFailed { .. } => Tone::Error,
        AgentEvent::RunStart { .. }
        | AgentEvent::RunEnd { .. }
        | AgentEvent::AgentStart { .. }
        | AgentEvent::TurnStart { .. }
        | AgentEvent::RequestStart { .. }
        | AgentEvent::MessageStart { .. }
        | AgentEvent::MessageUpdate { .. }
        | AgentEvent::MessageEnd { .. }
        | AgentEvent::ToolExecutionStart { .. }
        | AgentEvent::ToolExecutionUpdate { .. }
        | AgentEvent::ToolDecision { .. }
        | AgentEvent::ApprovalRequested { .. }
        | AgentEvent::ApprovalResolved { .. }
        | AgentEvent::ToolExecutionEnd { .. }
        | AgentEvent::TurnEnd { .. }
        | AgentEvent::AgentEnd { .. }
        | AgentEvent::ContextEdited { .. }
        | AgentEvent::CompactionStart { .. }
        | AgentEvent::CompactionEnd { .. } => Tone::Dim,
    }
}

/// The stderr line for `event` in text output, shared by `run` and chat. Names, arguments and
/// errors come from the model, servers or hooks, so they are escaped. Messages are left to
/// each caller, which shows answers in its own way.
pub(crate) fn progress_line(event: &AgentEvent) -> Option<String> {
    match event {
        AgentEvent::ToolExecutionStart { tool_name, args, .. } => Some(format!(
            "[tool] {} {}",
            visible(tool_name),
            visible(&clip(&args.to_string(), 300))
        )),
        AgentEvent::ToolExecutionUpdate { tool_name, content, .. } => {
            Some(format!("[tool] {}: {}", visible(tool_name), visible(content)))
        }
        AgentEvent::ToolExecutionEnd {
            tool_name,
            is_error,
            duration_ms,
            ..
        } => {
            let status = if *is_error { "error" } else { "ok" };
            let duration = duration_ms.map(|ms| format!(" {ms}ms")).unwrap_or_default();
            Some(format!("[tool] {} -> {status}{duration}", visible(tool_name)))
        }
        AgentEvent::RetryScheduled {
            attempt,
            max_retries,
            delay_ms,
            error,
        } => Some(format!(
            "[retry] {attempt}/{max_retries} in {delay_ms}ms: {}",
            visible(error)
        )),
        AgentEvent::ContextOverflow { error } => Some(format!(
            "[overflow] context too long; compacting before another attempt: {}",
            visible(error)
        )),
        AgentEvent::CompactionStart { input_bytes, .. } => {
            Some(format!("[compact] summarizing {input_bytes} context bytes"))
        }
        AgentEvent::CompactionEnd {
            input_bytes,
            output_bytes,
            ..
        } => Some(format!("[compact] {input_bytes} -> {output_bytes} context bytes")),
        AgentEvent::CompactionFailed { error, .. } => Some(format!("[compact] {}", visible(error))),
        AgentEvent::ToolCallRecovered {
            tool_call_id,
            tool_name,
            state,
        } => Some(format!(
            "[recover] {} {}: {}",
            visible(tool_name),
            visible(tool_call_id),
            state.describe()
        )),
        // The error names the hook and the phase: "<hook> after_tool failed: <reason>".
        AgentEvent::HookFailed { error, .. } => Some(format!("[hook] {}", visible(error))),
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
        | AgentEvent::ContextEdited { .. } => None,
    }
}

/// Escape control characters, including bidirectional overrides, so that text from models,
/// tools or servers cannot drive the terminal.
pub(crate) fn visible(text: &str) -> String {
    let mut visible = String::with_capacity(text.len());
    for c in text.chars() {
        if (c.is_control() && c != '\n' && c != '\t')
            || matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            visible.extend(c.escape_debug());
        } else {
            visible.push(c);
        }
    }
    visible
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn escapes_terminal_controls_and_bidi_markers_but_preserves_normal_unicode() {
        let text = visible("你好\n\t\u{1b}[2J\r\u{202e}cmd");
        assert!(text.starts_with("你好\n\t"));
        assert!(text.contains("\\u{1b}"));
        assert!(text.contains("\\r"));
        assert!(text.contains("\\u{202e}"));
        assert!(!text.contains('\u{1b}'));
    }

    #[test]
    fn a_call_is_described_by_its_main_argument_or_its_json() {
        assert_eq!(
            describe_call("bash", &json!({"command": "ls -la\nwc -l", "timeout": 5})),
            "bash: ls -la wc -l"
        );
        assert_eq!(
            describe_call("read_file", &json!({"path": "src/main.rs"})),
            "read_file: src/main.rs"
        );
        assert_eq!(
            describe_call("prom__query", &json!({"query": "up"})),
            "prom__query: {\"query\":\"up\"}"
        );
        assert_eq!(
            describe_call("bash", &json!("not an object")),
            "bash: \"not an object\""
        );
    }

    #[test]
    fn remaining_arguments_are_listed_only_when_the_label_left_some_out() {
        assert_eq!(
            remaining_arguments(&json!({"command": "ls", "timeout": 5})).as_deref(),
            Some("{\"timeout\":5}")
        );
        assert_eq!(remaining_arguments(&json!({"command": "ls"})), None);
        assert_eq!(remaining_arguments(&json!({"query": "up"})), None);
    }

    #[test]
    fn previews_show_the_first_lines_and_count_the_rest() {
        assert_eq!(preview(""), Vec::<String>::new());
        assert_eq!(preview("one\ntwo"), ["one", "two"]);
        let long = format!("{}\nb\nc\nd\ne", "a".repeat(PREVIEW_CHARS + 1));
        let lines = preview(&long);
        assert_eq!(lines.len(), PREVIEW_LINES + 1);
        assert_eq!(lines[0].chars().count(), PREVIEW_CHARS + 3, "{}", lines[0]);
        assert_eq!(lines[3], "… 2 more line(s)");
        assert_eq!(preview("\u{1b}[2Jx"), ["\\u{1b}[2Jx"]);
    }
}
