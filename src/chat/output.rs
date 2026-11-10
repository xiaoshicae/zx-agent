//! Streaming terminal output. Model text is rendered without executable control codes.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;
use zx_agent::agent::{AgentEvent, ApprovalResult, Event, EventSink, RequestKind};
use zx_agent::ai::{AssistantMessage, ContentBlock, Delta, Message, StopReason, ToolCall, ToolResultMessage};
use zx_agent::compaction::{self, Outcome};
use zx_agent::runner::RunReport;
use zx_agent::sync::lock;
use zx_agent::text::clip;

use crate::cli::{ColorArg, OutputMode};
use crate::process::{JsonlEvents, write_line};
use crate::render::{Severity, answer_end_line, call_label, progress_line, visible};

use super::debug::DebugState;
use super::term::{Phase, Style, Terminal, Tone, elapsed_text};

/// Lines of a tool result shown under its `[tool]` line.
const PREVIEW_LINES: usize = 3;
/// Lines of a patch shown under its `[tool]` line: a change deserves more than a result.
const PATCH_PREVIEW_LINES: usize = 20;
/// Characters kept of each previewed line.
const LINE_PREVIEW_CHARS: usize = 120;
/// What the conversation has used so far, for `/status`.
#[derive(Clone, Copy, Default)]
pub(super) struct Totals {
    pub(super) turns: u64,
    pub(super) requests: u64,
    pub(super) input_tokens: u64,
    pub(super) output_tokens: u64,
    pub(super) cached_tokens: u64,
    /// The cost of the responses that had prices; `None` until one had.
    pub(super) total_cost: Option<f64>,
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
                if let Some(cost) = report.usage.total_cost {
                    totals.total_cost = Some(totals.total_cost.unwrap_or(0.0) + cost);
                }
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

    /// Finish the answer: text that was not streamed, labels for its calls, and how it ended.
    fn answer_ended(&self, state: &mut TextState, message: &AssistantMessage) {
        state.terminal.end_aside();
        let text = message.text();
        if !state.wrote_text && !text.is_empty() {
            state.terminal.answer_text(&visible(&text));
        }
        state.terminal.end_answer();
        for block in &message.content {
            if let ContentBlock::ToolCall(ToolCall {
                id, name, arguments, ..
            }) = block
            {
                state.labels.insert(id.clone(), call_label(name, arguments));
            }
        }
        if let Some(error) = &message.error_message {
            state.last_error = Some(error.clone());
        }
        if self.debug {
            return;
        }
        // A cancelled request is reported once, with the run's report.
        if message.stop_reason != StopReason::Aborted
            && let Some((severity, line)) = answer_end_line(message)
        {
            state.terminal.line(tone(severity), &line);
        }
    }

    /// Retry `attempt` of `max_retries` is scheduled: say so, unless the failed response just
    /// said it, and show the wait in the status line.
    fn retry_scheduled(&self, state: &mut TextState, attempt: u32, max_retries: u32, delay_ms: u64, error: &str) {
        if !self.debug {
            // The failed response was reported just before; name it only once.
            let line = if state.last_error.as_deref() == Some(error) {
                format!("[retry] {attempt}/{max_retries} in {delay_ms}ms (same error)")
            } else {
                format!("[retry] {attempt}/{max_retries} in {delay_ms}ms: {}", visible(error))
            };
            state.terminal.line(Tone::Warn, &line);
        }
        state.terminal.set_phase(Phase::Retry { attempt, max_retries });
    }

    /// An event with no line of chat's own: print it as `run` does, and clear the status line
    /// when a summary is over, as a manual one ends without a `run_end`.
    fn progress(&self, state: &mut TextState, event: &AgentEvent) {
        if !self.debug
            && let Some((severity, line)) = progress_line(event)
        {
            state.terminal.line(tone(severity), &line);
        }
        if matches!(
            event,
            AgentEvent::CompactionEnd { .. } | AgentEvent::CompactionFailed { .. }
        ) {
            state.terminal.clear_status();
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

/// A call starts: the status line names it, with the label its call was shown with.
fn tool_started(state: &mut TextState, call: &str, tool: &str, args: &Value) {
    let label = state
        .labels
        .get(call)
        .cloned()
        .unwrap_or_else(|| call_label(tool, args));
    state.terminal.set_phase(Phase::Tool { label });
}

/// A message an extension sent and asked to show: its type, then a preview of its content.
fn custom_message(state: &mut TextState, custom_type: &str, content: &str) {
    let style = state.terminal.style();
    state.terminal.line(Tone::Dim, &format!("[{}]", visible(custom_type)));
    for line in limited(content, PREVIEW_LINES, style, |_| Tone::Plain) {
        state.terminal.line(Tone::Plain, &format!("  {line}"));
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
                } => self.answer_ended(&mut state, message),
                AgentEvent::MessageEnd {
                    message: Message::ToolResult(result),
                    ..
                } => {
                    if !self.debug {
                        self.tool_result(&mut state, result);
                    }
                }
                // A message an extension sent, shown when it asks for that (pi: `display`).
                AgentEvent::MessageEnd {
                    message: Message::User { content, .. },
                    custom: Some(info),
                    ..
                } if info.display && !self.debug => custom_message(&mut state, &info.custom_type, content),
                AgentEvent::ToolExecutionStart {
                    tool_call_id,
                    tool_name,
                    args,
                } => tool_started(&mut state, tool_call_id, tool_name, args),
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
                } => self.retry_scheduled(&mut state, *attempt, *max_retries, *delay_ms, error),
                // Everything else is printed as `run` prints it; `progress_line` covers every event.
                other => self.progress(&mut state, other),
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
    let cost = report.usage.total_cost.map(cost_text).unwrap_or_default();
    format!("[turn] {requests}{retries}{usage}{cost} · {}", elapsed_text(elapsed))
}

/// ` · cost 0.0123`, in the prices' unit, whatever the currency: four decimals, since a turn
/// costs a small fraction of a unit.
pub(super) fn cost_text(total_cost: f64) -> String {
    format!(" · cost {total_cost:.4}")
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
        .map(|line| style.paint(tone(line), &clip(&visible(line), LINE_PREVIEW_CHARS)))
        .collect();
    if total > max_lines {
        lines.push(style.paint(Tone::Dim, &format!("… {} more line(s)", total - max_lines)));
    }
    lines
}

/// The tone of a line by how serious it is: progress is dimmed.
fn tone(severity: Severity) -> Tone {
    match severity {
        Severity::Info => Tone::Dim,
        Severity::Warn => Tone::Warn,
        Severity::Error => Tone::Error,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

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
            tool_usage: std::collections::BTreeMap::new(),
            denied_calls: 0,
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
            usage: None,
        }
    }

    #[test]
    fn previews_show_the_first_lines_and_count_the_rest() {
        let plain = Style::new(false);
        assert_eq!(preview(&result("", None), plain), Vec::<String>::new());
        assert_eq!(preview(&result("one\ntwo", None), plain), ["one", "two"]);
        let long = format!("{}\nb\nc\nd\ne", "a".repeat(LINE_PREVIEW_CHARS + 1));
        let lines = preview(&result(&long, None), plain);
        assert_eq!(lines.len(), PREVIEW_LINES + 1);
        assert_eq!(lines[0].chars().count(), LINE_PREVIEW_CHARS + 3, "{}", lines[0]);
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
