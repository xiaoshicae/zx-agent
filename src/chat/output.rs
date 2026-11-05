//! Streaming terminal output. Model text is rendered without executable control codes.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;
use zx_agent::agent::{AgentEvent, ApprovalResult, Event, EventSink, RequestKind};
use zx_agent::ai::{ContentBlock, Delta, Message, StopReason, ToolResultMessage};
use zx_agent::compaction::{self, Outcome};
use zx_agent::runner::RunReport;
use zx_agent::sync::lock;
use zx_agent::text::clip;

use crate::cli::{ColorArg, OutputMode};
use crate::{JsonlEvents, write_line};

use super::debug::DebugState;
use super::term::{Phase, Style, Terminal, Tone, elapsed_text};

/// Lines of a tool result shown under its `[tool]` line.
const PREVIEW_LINES: usize = 3;
/// Lines of a patch shown under its `[tool]` line: a change deserves more than a result.
const PATCH_PREVIEW_LINES: usize = 20;
/// Characters kept of each previewed line.
const PREVIEW_CHARS: usize = 120;
/// Characters of an argument string shown inline in an approval; longer or multi-line ones
/// become indented blocks.
const INLINE_CHARS: usize = 80;
/// Lines of one argument string shown in an approval. The rest is counted, so a person
/// knows what was not shown before approving.
const BLOCK_LINES: usize = 60;
/// Characters of a call's label in progress and result lines; approvals show everything.
const LABEL_CHARS: usize = 80;
/// Argument fields that say what a call does, shown instead of the whole JSON: the first
/// one present is the call's main value, the others follow as `name=value`.
const LABEL_FIELDS: [&str; 3] = ["command", "pattern", "path"];

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
    /// The last model error printed in this run, so a retry line need not repeat it.
    last_error: Option<String>,
}

pub(super) struct Output {
    mode: OutputMode,
    debug: bool,
    /// Stream reasoning, dimmed, on stderr.
    show_thinking: bool,
    debug_state: Mutex<DebugState>,
    json: JsonlEvents,
    text: Mutex<TextState>,
}

impl Output {
    pub(super) fn new(mode: OutputMode, deltas: bool, debug: bool, color: ColorArg, show_thinking: bool) -> Self {
        let stderr_is_terminal = std::io::stderr().is_terminal();
        let style = match color {
            ColorArg::Auto => Style::auto(stderr_is_terminal),
            ColorArg::Always => Style::new(true),
            ColorArg::Never => Style::new(false),
        };
        // A status line is redrawn in place, which a dumb terminal or a log cannot show; the
        // debug timeline prints every step itself.
        let dumb = std::env::var_os("TERM").is_some_and(|value| value == "dumb");
        let live = stderr_is_terminal && !dumb && !debug && mode == OutputMode::Text;
        let spaced = std::io::stdout().is_terminal();
        Self {
            mode,
            debug,
            show_thinking,
            debug_state: Mutex::new(DebugState::default()),
            json: JsonlEvents { deltas },
            text: Mutex::new(TextState {
                terminal: Terminal::new(style, live, spaced),
                labels: HashMap::new(),
                wrote_text: false,
                totals: Totals::default(),
                last_error: None,
            }),
        }
    }

    pub(super) fn style(&self) -> Style {
        lock(&self.text).terminal.style()
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

    /// Drop the status line: the operation it described is over.
    pub(super) fn clear_status(&self) {
        lock(&self.text).terminal.clear_status();
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
                state.terminal.line(Tone::Dim, &turn_line(report, elapsed));
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
        let style = state.terminal.style();
        let status = if result.is_error {
            style.paint(Tone::Error, "error")
        } else {
            style.paint(Tone::Ok, "ok")
        };
        let duration = result.duration_ms.map(|ms| format!(" {ms}ms")).unwrap_or_default();
        let line = format!(
            "{} {label} -> {status}{}",
            style.paint(Tone::Dim, "[tool]"),
            style.paint(Tone::Dim, &duration)
        );
        state.terminal.tool_done(Tone::Plain, &line);
        for line in preview(result, style) {
            state.terminal.line(Tone::Plain, &format!("    {line}"));
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
                AgentEvent::RunStart { .. } => state.last_error = None,
                AgentEvent::RequestStart { kind, .. } => state.terminal.set_phase(match kind {
                    RequestKind::Task => Phase::Model { thinking: false },
                    RequestKind::Summary => Phase::Summary,
                }),
                AgentEvent::MessageStart { role: "assistant" } => state.wrote_text = false,
                AgentEvent::MessageUpdate {
                    delta: Delta::Text { text },
                } => {
                    state.terminal.answer_text(&visible(text));
                    state.wrote_text = true;
                }
                AgentEvent::MessageUpdate {
                    delta: Delta::Thinking { text },
                } => {
                    if self.show_thinking {
                        state.terminal.aside_text(&visible(text));
                    } else {
                        state.terminal.set_phase(Phase::Model { thinking: true });
                    }
                }
                AgentEvent::MessageEnd {
                    message: Message::Assistant(message),
                    ..
                } => {
                    state.terminal.end_aside();
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
                    if let Some(error) = &message.error_message {
                        state.last_error = Some(error.clone());
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
                AgentEvent::RetryScheduled {
                    attempt,
                    max_retries,
                    delay_ms,
                    error,
                } => {
                    if !self.debug {
                        // The failed response was reported just before; name it only once.
                        let line = if state.last_error.as_deref() == Some(error.as_str()) {
                            format!("[retry] {attempt}/{max_retries} in {delay_ms}ms (same error)")
                        } else {
                            format!("[retry] {attempt}/{max_retries} in {delay_ms}ms: {}", visible(error))
                        };
                        state.terminal.line(Tone::Warn, &line);
                    }
                    state.terminal.set_phase(Phase::Retry {
                        attempt: *attempt,
                        max_retries: *max_retries,
                    });
                }
                // Everything else is printed as `run` prints it; `progress_line` covers every event.
                other => {
                    if !self.debug
                        && let Some(line) = progress_line(other)
                    {
                        state.terminal.line(progress_tone(other), &line);
                    }
                    // A summary is over: a manual one ends without a `run_end`.
                    if matches!(
                        other,
                        AgentEvent::CompactionEnd { .. } | AgentEvent::CompactionFailed { .. }
                    ) {
                        state.terminal.clear_status();
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

/// The line that ends a turn: requests, retries, tokens when the provider reported any, and
/// the time it took.
fn turn_line(report: &RunReport, elapsed: Duration) -> String {
    let requests = match report.model_requests {
        1 => "1 request".to_string(),
        n => format!("{n} requests"),
    };
    let retries = match report.retries {
        0 => String::new(),
        1 => " · 1 retry".to_string(),
        n => format!(" · {n} retries"),
    };
    // Zero for both means the provider sent no usage, or the request was cancelled.
    let usage = if report.usage.input == 0 && report.usage.output == 0 {
        String::new()
    } else {
        let cached = match report.usage.cache_read {
            0 => String::new(),
            n => format!(" · {n} cached"),
        };
        format!(" · {} in · {} out{cached}", report.usage.input, report.usage.output)
    };
    format!("[turn] {requests}{retries}{usage} · {}", elapsed_text(elapsed))
}

/// `name: <main argument> other=value`, or `name: <arguments as JSON>`; nothing is cut,
/// control characters are escaped and a multi-line command stays on one line.
pub(super) fn describe_call(name: &str, args: &Value) -> String {
    let mut labels = label_fields(args);
    let detail = match labels.next() {
        Some((_, main)) => {
            let mut detail = main.to_string();
            for (field, value) in labels {
                detail.push_str(&format!(" {field}={value}"));
            }
            detail
        }
        None => args.to_string(),
    };
    format!("{}: {}", visible(name), visible(&detail).replace('\n', " "))
}

/// The label fields `args` carries as strings, in [`LABEL_FIELDS`] order.
fn label_fields(args: &Value) -> impl Iterator<Item = (&'static str, &str)> {
    LABEL_FIELDS.iter().filter_map(move |field| {
        args.as_object()
            .and_then(|fields| fields.get(*field))
            .and_then(Value::as_str)
            .map(|value| (*field, value))
    })
}

/// The arguments [`describe_call`] left out, readable: scalars inline, long or multi-line
/// strings as indented blocks with their line count, arrays and objects by path. Nothing is
/// escaped as JSON, so a file to be written looks like the file.
pub(super) fn describe_arguments(args: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    match args.as_object() {
        Some(fields) => {
            let shown: Vec<&str> = label_fields(args).map(|(field, _)| field).collect();
            for (key, value) in fields {
                if !shown.contains(&key.as_str()) {
                    describe_value(key, value, &mut lines);
                }
            }
        }
        None => describe_value("arguments", args, &mut lines),
    }
    lines
}

fn describe_value(path: &str, value: &Value, lines: &mut Vec<String>) {
    match value {
        Value::String(text) if text.contains('\n') || text.chars().count() > INLINE_CHARS => {
            let total = text.lines().count();
            let plural = if total == 1 { "" } else { "s" };
            lines.push(format!("{path} ({total} line{plural}):"));
            for line in text.lines().take(BLOCK_LINES) {
                lines.push(format!("    {}", visible(line)));
            }
            if total > BLOCK_LINES {
                lines.push(format!("    … {} more line(s) not shown", total - BLOCK_LINES));
            }
        }
        Value::String(text) => lines.push(format!("{path}: {}", visible(text))),
        Value::Array(items) if items.is_empty() => lines.push(format!("{path}: []")),
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                describe_value(&format!("{path}[{index}]"), item, lines);
            }
        }
        Value::Object(fields) if fields.is_empty() => lines.push(format!("{path}: {{}}")),
        Value::Object(fields) => {
            for (key, item) in fields {
                describe_value(&format!("{path}.{key}"), item, lines);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => lines.push(format!("{path}: {value}")),
    }
}

/// Lines shown under a result line, painted: the first lines of the patch a tool put in its
/// details (what `edit_file` changed), otherwise the first lines of its text.
fn preview(result: &ToolResultMessage, style: Style) -> Vec<String> {
    let patch = result
        .details
        .as_ref()
        .and_then(|details| details.get("patch"))
        .and_then(Value::as_str);
    match patch {
        Some(patch) => limited(patch, PATCH_PREVIEW_LINES, style, |line| {
            // Unified diff: added and removed lines stand out, headers and context do not.
            if line.starts_with('+') && !line.starts_with("+++") {
                Tone::Ok
            } else if line.starts_with('-') && !line.starts_with("---") {
                Tone::Error
            } else {
                Tone::Dim
            }
        }),
        None => limited(&result.content, PREVIEW_LINES, style, |_| Tone::Dim),
    }
}

/// The first `max_lines` of `text`, each cut short and painted, then how many more there are.
fn limited(text: &str, max_lines: usize, style: Style, tone: impl Fn(&str) -> Tone) -> Vec<String> {
    let total = text.lines().count();
    let mut lines: Vec<String> = text
        .lines()
        .take(max_lines)
        .map(|line| style.paint(tone(line), &clip(&visible(line), PREVIEW_CHARS)))
        .collect();
    if total > max_lines {
        lines.push(style.paint(Tone::Dim, &format!("… {} more line(s)", total - max_lines)));
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
            describe_call(
                "grep",
                &json!({"pattern": "fn main", "path": "src", "ignoreCase": true})
            ),
            "grep: fn main path=src"
        );
        assert_eq!(describe_call("ls", &json!({})), "ls: {}");
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
    fn arguments_left_out_of_the_label_are_listed_readably() {
        assert_eq!(
            describe_arguments(&json!({"command": "ls", "timeout": 5})),
            ["timeout: 5"]
        );
        assert_eq!(describe_arguments(&json!({"command": "ls"})), Vec::<String>::new());
        assert_eq!(
            describe_arguments(&json!({"pattern": "x", "path": "src", "limit": 5})),
            ["limit: 5"]
        );
        assert_eq!(
            describe_arguments(&json!({"path": "notes.txt", "content": "one\ntwo\n"})),
            ["content (2 lines):", "    one", "    two"]
        );
        assert_eq!(
            describe_arguments(&json!({"path": "a.rs", "edits": [{"oldText": "x", "newText": "y\u{1b}"}]})),
            ["edits[0].newText: y\\u{1b}", "edits[0].oldText: x"]
        );
        assert_eq!(
            describe_arguments(&json!({"query": "up", "flags": [], "extra": {}, "on": true, "n": null})),
            ["extra: {}", "flags: []", "n: null", "on: true", "query: up"]
        );
        assert_eq!(describe_arguments(&json!("text")), ["arguments: text"]);
        let long = "x".repeat(INLINE_CHARS + 1);
        assert_eq!(
            describe_arguments(&json!({"note": long})),
            ["note (1 line):".to_string(), format!("    {long}")]
        );
        let tall = (0..BLOCK_LINES + 2)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let lines = describe_arguments(&json!({"content": tall}));
        assert_eq!(lines.len(), BLOCK_LINES + 2);
        assert_eq!(lines[BLOCK_LINES + 1], "    … 2 more line(s) not shown");
    }

    #[test]
    fn the_turn_line_omits_tokens_the_provider_did_not_report() {
        use zx_agent::agent::RunOutcome;
        use zx_agent::ai::Usage;
        let mut report = RunReport {
            outcome: RunOutcome::Completed,
            final_text: String::new(),
            model_requests: 2,
            retries: 1,
            usage: Usage::default(),
            error: None,
            session_file: None,
        };
        assert_eq!(
            turn_line(&report, Duration::from_millis(600)),
            "[turn] 2 requests · 1 retry · 0.6s"
        );
        report.usage.input = 30;
        report.usage.output = 15;
        report.usage.cache_read = 10;
        report.retries = 0;
        report.model_requests = 1;
        assert_eq!(
            turn_line(&report, Duration::from_secs(12)),
            "[turn] 1 request · 30 in · 15 out · 10 cached · 12s"
        );
    }

    fn result(content: &str, details: Option<Value>) -> ToolResultMessage {
        ToolResultMessage {
            tool_call_id: "c".into(),
            tool_name: "t".into(),
            content: content.into(),
            is_error: false,
            timestamp: 0,
            duration_ms: None,
            details,
        }
    }

    #[test]
    fn previews_show_the_first_lines_and_count_the_rest() {
        let plain = Style::new(false);
        assert_eq!(preview(&result("", None), plain), Vec::<String>::new());
        assert_eq!(preview(&result("one\ntwo", None), plain), ["one", "two"]);
        let long = format!("{}\nb\nc\nd\ne", "a".repeat(PREVIEW_CHARS + 1));
        let lines = preview(&result(&long, None), plain);
        assert_eq!(lines.len(), PREVIEW_LINES + 1);
        assert_eq!(lines[0].chars().count(), PREVIEW_CHARS + 3, "{}", lines[0]);
        assert_eq!(lines[3], "… 2 more line(s)");
        assert_eq!(preview(&result("\u{1b}[2Jx", None), plain), ["\\u{1b}[2Jx"]);
    }

    #[test]
    fn a_patch_in_the_details_is_previewed_instead_of_the_text_with_changes_colored() {
        let patch = "--- f\n+++ f\n@@ -1,2 +1,2 @@\n-old\n+new\n same\n";
        let message = result("Replaced 1 block in f.", Some(json!({"patch": patch, "path": "f"})));
        assert_eq!(
            preview(&message, Style::new(false)),
            ["--- f", "+++ f", "@@ -1,2 +1,2 @@", "-old", "+new", " same"]
        );
        let colored = preview(&message, Style::new(true));
        assert_eq!(colored[0], "\x1b[2m--- f\x1b[0m");
        assert_eq!(colored[3], "\x1b[31m-old\x1b[0m");
        assert_eq!(colored[4], "\x1b[32m+new\x1b[0m");
    }
}
