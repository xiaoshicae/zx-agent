//! Streaming terminal output. Model text is rendered without executable control codes.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

use zx_agent::agent::{AgentEvent, EventSink};
use zx_agent::ai::{Delta, Message, StopReason};
use zx_agent::compaction::{self, Outcome};
use zx_agent::runner::RunReport;

use crate::cli::OutputMode;
use crate::{JsonlEvents, clip, print_report, write_line};

pub(super) struct Output {
    mode: OutputMode,
    wrote_text: AtomicBool,
    json: JsonlEvents,
}

impl Output {
    pub(super) fn new(mode: OutputMode, deltas: bool) -> Self {
        Self {
            mode,
            wrote_text: AtomicBool::new(false),
            json: JsonlEvents { deltas },
        }
    }

    pub(super) fn report(&self, report: &RunReport) {
        match self.mode {
            OutputMode::Jsonl => print_report(report, OutputMode::Jsonl),
            OutputMode::Text => {
                if let Some(error) = &report.error {
                    eprintln!("[chat] {}", visible(error));
                }
            }
        }
    }

    pub(super) fn compact_report(&self, report: &compaction::Report) {
        match self.mode {
            OutputMode::Jsonl => write_line(&serde_json::json!({"type":"compaction_report","report":report})),
            OutputMode::Text => match report.outcome {
                Outcome::Noop => eprintln!("[compact] no eligible old messages to summarize"),
                Outcome::Completed => eprintln!(
                    "[compact] requests={} tokens={}",
                    report.requests, report.usage.total_tokens
                ),
                Outcome::Failed | Outcome::Aborted => {
                    if let Some(error) = &report.error {
                        eprintln!("[compact] {}", visible(error));
                    }
                }
            },
        }
    }
}

impl EventSink for Output {
    fn emit(&self, event: AgentEvent) {
        if self.mode == OutputMode::Jsonl {
            self.json.emit(event);
            return;
        }
        match &event {
            AgentEvent::MessageStart { role: "assistant" } => self.wrote_text.store(false, Ordering::SeqCst),
            AgentEvent::MessageUpdate {
                delta: Delta::Text { text },
            } => {
                write_stdout(&visible(text));
                self.wrote_text.store(true, Ordering::SeqCst);
            }
            AgentEvent::MessageEnd {
                message: Message::Assistant(message),
            } => {
                let text = message.text();
                if !self.wrote_text.load(Ordering::SeqCst) && !text.is_empty() {
                    write_stdout(&visible(&text));
                }
                if self.wrote_text.load(Ordering::SeqCst) || !text.is_empty() {
                    write_stdout("\n");
                }
                if let Some(error) = &message.error_message {
                    eprintln!("[model] {:?}: {}", message.stop_reason, visible(error));
                } else if message.stop_reason == StopReason::Length {
                    eprintln!("[model] response reached the output token limit");
                }
            }
            AgentEvent::ToolExecutionStart { tool_name, args, .. } => eprintln!(
                "[tool] {} {}",
                visible(tool_name),
                visible(&clip(&args.to_string(), 300))
            ),
            AgentEvent::ToolExecutionEnd {
                tool_name,
                is_error,
                duration_ms,
                ..
            } => {
                let status = if *is_error { "error" } else { "ok" };
                let duration = duration_ms.map(|ms| format!(" {ms}ms")).unwrap_or_default();
                eprintln!("[tool] {} -> {status}{duration}", visible(tool_name));
            }
            AgentEvent::RetryScheduled {
                attempt,
                max_retries,
                delay_ms,
                error,
            } => eprintln!("[retry] {attempt}/{max_retries} in {delay_ms}ms: {}", visible(error)),
            AgentEvent::CompactionStart { input_bytes, .. } => {
                eprintln!("[compact] summarizing {input_bytes} context bytes")
            }
            AgentEvent::CompactionEnd {
                input_bytes,
                output_bytes,
                ..
            } => eprintln!("[compact] {input_bytes} -> {output_bytes} context bytes"),
            AgentEvent::CompactionFailed { error, .. } => eprintln!("[compact] {}", visible(error)),
            AgentEvent::ToolCallRecovered {
                tool_call_id,
                tool_name,
                state,
            } => eprintln!(
                "[recover] {} {}: {}",
                visible(tool_name),
                visible(tool_call_id),
                state.describe()
            ),
            AgentEvent::AgentStart
            | AgentEvent::TurnStart { .. }
            | AgentEvent::MessageStart { .. }
            | AgentEvent::MessageUpdate { .. }
            | AgentEvent::MessageEnd { .. }
            | AgentEvent::TurnEnd { .. }
            | AgentEvent::AgentEnd { .. }
            | AgentEvent::ContextEdited { .. }
            | AgentEvent::HookFailed { .. } => {}
        }
    }
}

fn write_stdout(text: &str) {
    // Short bounded chunks, like the existing synchronous CLI event sink.
    let mut stdout = std::io::stdout().lock();
    // A closed consumer has nobody to receive more text; prompts still return a report.
    let _ = stdout.write_all(text.as_bytes()).and_then(|()| stdout.flush());
}

pub(super) fn visible(text: &str) -> String {
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
}
