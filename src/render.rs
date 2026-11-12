//! Text about calls and events for a terminal or a log, shared by run, chat, acp and the
//! session listing. Names, arguments and errors come from models, tools and servers, so
//! nothing printed here can drive the terminal.

use serde_json::Value;
use zx_agent::agent::{AgentEvent, Interruption};
use zx_agent::ai::{AssistantMessage, StopReason};
use zx_agent::text::clip;

/// Characters of an argument string shown inline in an approval; longer or multi-line ones
/// become indented blocks.
const INLINE_CHARS: usize = 80;
/// Lines of one argument string shown in an approval. The rest is counted, so a person
/// knows what was not shown before approving.
const BLOCK_LINES: usize = 60;
/// Argument fields that say what a call does, shown instead of the whole JSON: the first
/// one present is the call's main value, the others follow as `name=value`.
const LABEL_FIELDS: [&str; 3] = ["command", "pattern", "path"];
/// Characters of a call's arguments on a `[tool]` progress line.
const PROGRESS_ARGS_CHARS: usize = 300;
/// Characters of a call's label on a progress or result line, or as a client's title;
/// approvals show everything.
const LABEL_CHARS: usize = 80;

/// [`describe_call`] cut to [`LABEL_CHARS`], for a line that lists the call.
pub(crate) fn call_label(name: &str, args: &Value) -> String {
    clip(&describe_call(name, args), LABEL_CHARS)
}

/// `name: <main argument> other=value`, or `name: <arguments as JSON>`; nothing is cut,
/// control characters are escaped and a multi-line command stays on one line.
pub(crate) fn describe_call(name: &str, args: &Value) -> String {
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
pub(crate) fn describe_arguments(args: &Value) -> Vec<String> {
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

/// How serious a line is, for a terminal that colors lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Severity {
    /// Progress and housekeeping.
    Info,
    /// A recoverable problem.
    Warn,
    /// A failure.
    Error,
}

/// The line about an answer that ended badly, with how serious that is: its error, or the
/// output token limit it hit.
pub(crate) fn answer_end_line(message: &AssistantMessage) -> Option<(Severity, String)> {
    if let Some(error) = &message.error_message {
        Some((
            Severity::Error,
            format!("[model] {:?}: {}", message.stop_reason, visible(error)),
        ))
    } else if message.stop_reason == StopReason::Length {
        Some((
            Severity::Warn,
            "[model] Length: the response hit the output token limit".to_string(),
        ))
    } else {
        None
    }
}

/// The stderr line for `event` in text output, with how serious it is, shared by `run` and
/// chat. Names, arguments and errors come from the model, servers or hooks, so they are
/// escaped. Messages are left to each caller, which shows answers in its own way.
#[expect(clippy::too_many_lines, reason = "one arm per event variant")]
pub(crate) fn progress_line(event: &AgentEvent) -> Option<(Severity, String)> {
    let line = match event {
        AgentEvent::ToolExecutionStart { tool_name, args, .. } => (
            Severity::Info,
            format!(
                "[tool] {} {}",
                visible(tool_name),
                visible(&clip(&args.to_string(), PROGRESS_ARGS_CHARS))
            ),
        ),
        AgentEvent::ToolExecutionUpdate { tool_name, content, .. } => (
            Severity::Info,
            format!("[tool] {}: {}", visible(tool_name), visible(content)),
        ),
        AgentEvent::ToolExecutionEnd {
            tool_name,
            is_error,
            duration_ms,
            ..
        } => {
            let status = if *is_error { "error" } else { "ok" };
            let duration = duration_ms.map(|ms| format!(" {ms}ms")).unwrap_or_default();
            (
                Severity::Info,
                format!("[tool] {} -> {status}{duration}", visible(tool_name)),
            )
        }
        AgentEvent::RetryScheduled {
            retry,
            max_retries,
            delay_ms,
            error,
        } => (
            Severity::Warn,
            format!("[retry] {retry}/{max_retries} in {delay_ms}ms: {}", visible(error)),
        ),
        AgentEvent::ContextOverflow { error } => (
            Severity::Warn,
            format!(
                "[overflow] context too long; compacting before another attempt: {}",
                visible(error)
            ),
        ),
        AgentEvent::CompactionStart { input_bytes, .. } => (
            Severity::Info,
            format!("[compact] summarizing {input_bytes} context bytes"),
        ),
        AgentEvent::CompactionEnd {
            input_bytes,
            output_bytes,
            ..
        } => (
            Severity::Info,
            format!("[compact] {input_bytes} -> {output_bytes} context bytes"),
        ),
        AgentEvent::CompactionFailed { error, .. } => (Severity::Error, format!("[compact] {}", visible(error))),
        AgentEvent::ToolCallRecovered {
            tool_call_id,
            tool_name,
            state,
        } => (
            Severity::Warn,
            format!(
                "[recover] {} {}: {}",
                visible(tool_name),
                visible(tool_call_id),
                describe_interruption(*state)
            ),
        ),
        // The error names the hook and the phase: "<hook> after_tool failed: <reason>".
        AgentEvent::HookFailed { error, .. } => (Severity::Error, format!("[hook] {}", visible(error))),
        AgentEvent::CustomEntry { custom_type, .. } => {
            (Severity::Info, format!("[extension] recorded {}", visible(custom_type)))
        }
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
        | AgentEvent::ContextEdited { .. } => return None,
    };
    Some(line)
}

/// What is known about a call that an interrupted run left without a result, for a
/// progress line.
fn describe_interruption(state: Interruption) -> &'static str {
    match state {
        Interruption::Started => "started, result unknown",
        Interruption::StartUnknown => "start not recorded, result unknown",
        Interruption::NotStarted => "not started",
        Interruption::ReadOnly => "read-only, no result",
    }
}

/// `ms` since the epoch in local time, written as `format` (chrono's strftime), or `None`
/// for a time the calendar cannot hold.
pub(crate) fn local_time(ms: u64, format: &str) -> Option<String> {
    // A timestamp beyond i64, or past the year 262143, cannot occur; each caller shows its
    // line without the time.
    i64::try_from(ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|utc| utc.with_timezone(&chrono::Local).format(format).to_string())
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
    fn a_label_is_the_description_cut_short() {
        assert_eq!(call_label("bash", &json!({"command": "ls -la"})), "bash: ls -la");
        let long = call_label("bash", &json!({"command": "x".repeat(LABEL_CHARS)}));
        assert_eq!(long, format!("bash: {}...", "x".repeat(LABEL_CHARS - "bash: ".len())));
    }

    // Fields keep the order they were sent in: serde_json's `preserve_order` is on for the
    // whole workspace since the ACP SDK enables it.
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
            ["edits[0].oldText: x", "edits[0].newText: y\\u{1b}"]
        );
        assert_eq!(
            describe_arguments(&json!({"query": "up", "flags": [], "extra": {}, "on": true, "n": null})),
            ["query: up", "flags: []", "extra: {}", "on: true", "n: null"]
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
}
