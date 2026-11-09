//! Human-readable execution timeline for chat's text debug mode.

use std::time::Instant;

use zx_agent::agent::{AgentEvent, ApprovalResult, Event, RequestKind, RunOutcome, RunReport, ToolVerdict};
use zx_agent::ai::{AssistantMessage, ContentBlock, Message, StopReason, ToolResultMessage, Usage};

use crate::render::{Severity, local_time, visible};

/// How the timeline shows when an event happened.
const TIMELINE_TIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S%.3f %:z";

#[derive(Default)]
pub(super) struct DebugState {
    run_id: String,
    turn: Option<u32>,
    requests: u64,
    request_started: Option<Instant>,
    last_error: Option<String>,
}

impl DebugState {
    /// Print the line for `event`, if it has one; `color` is the frontend's decision.
    pub(super) fn emit(&mut self, event: &Event, color: bool) {
        let Some((severity, text)) = self.describe(event) else {
            return;
        };
        eprintln!("{}", format_line(event.at_ms, severity, &text, color));
    }

    #[expect(clippy::too_many_lines, reason = "one arm per event variant")]
    fn describe(&mut self, event: &Event) -> Option<(Severity, String)> {
        use AgentEvent::{
            AgentEnd, AgentStart, ApprovalRequested, ApprovalResolved, CompactionEnd, CompactionFailed,
            CompactionStart, ContextEdited, ContextOverflow, CustomEntry, HookFailed, MessageEnd, MessageStart,
            MessageUpdate, RequestStart, RetryScheduled, RunEnd, RunStart, ToolCallRecovered, ToolDecision,
            ToolExecutionEnd, ToolExecutionStart, ToolExecutionUpdate, TurnEnd, TurnStart,
        };
        if self.run_id != event.run_id {
            *self = Self {
                run_id: event.run_id.clone(),
                ..Self::default()
            };
        }
        let item = match &event.event {
            RunStart { model, provider } => (
                Severity::Info,
                format!(
                    "Task started · model={model} · provider={provider} · run={} · session={}",
                    event.run_id,
                    event.session_id.as_deref().unwrap_or("none")
                ),
            ),
            AgentStart { .. } | AgentEnd { .. } | MessageStart { .. } | MessageUpdate { .. } => return None,
            TurnStart { turn } => {
                self.turn = Some(*turn);
                return None;
            }
            TurnEnd { .. } => {
                self.turn = None;
                return None;
            }
            RequestStart {
                kind,
                entry_id,
                model,
                attempt,
            } => self.request(*kind, entry_id, model, *attempt),
            MessageEnd {
                message: Message::Assistant(message),
                ..
            } => self.response(message),
            CustomEntry { entry_id, custom_type } => {
                recorded(&format!("Extension state recorded · type={custom_type}"), entry_id)
            }
            MessageEnd {
                message: Message::ToolResult(result),
                entry_id,
                ..
            } => tool_result(result, entry_id),
            MessageEnd {
                message: Message::User { kind, .. },
                entry_id,
                ..
            } => recorded(&format!("User input recorded · kind={kind:?}"), entry_id),
            MessageEnd {
                message: Message::System(_),
                entry_id,
                ..
            } => recorded("System prompt changes recorded", entry_id),
            ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => (
                Severity::Info,
                format!(
                    "Tool call · {tool_name} · call={tool_call_id}\n  arguments: {}",
                    serde_json::to_string_pretty(args).unwrap_or_else(|_| args.to_string())
                ),
            ),
            ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                content,
            } => (
                Severity::Info,
                format!("Tool progress · {tool_name} · call={tool_call_id}\n  {content}"),
            ),
            ToolDecision {
                tool_call_id,
                tool_name,
                verdict,
                reason,
            } => tool_decision(tool_call_id, tool_name, *verdict, reason.as_deref()),
            ApprovalRequested {
                tool_call_id,
                tool_name,
                reason,
            } => (
                Severity::Warn,
                format!("Approval requested · {tool_name} · call={tool_call_id} · {reason}"),
            ),
            ApprovalResolved {
                tool_call_id,
                tool_name,
                approval,
                reason,
                wait_ms,
            } => approval_resolved(tool_call_id, tool_name, *approval, reason.as_deref(), *wait_ms),
            ToolExecutionEnd {
                tool_call_id,
                tool_name,
                is_error,
                duration_ms,
                details: _,
                usage: _,
            } => tool_finished(tool_call_id, tool_name, *is_error, *duration_ms),
            RetryScheduled {
                attempt,
                max_retries,
                delay_ms,
                error,
            } => (
                Severity::Warn,
                format!(
                    "Retry {attempt}/{max_retries} in {delay_ms}ms{}",
                    self.unless_last_error(error)
                ),
            ),
            ContextOverflow { error } => (
                Severity::Warn,
                format!(
                    "Context too long for the model; compacting before another attempt{}",
                    self.unless_last_error(error)
                ),
            ),
            ContextEdited {
                index,
                replacement,
                entry_id,
            } => (
                Severity::Info,
                format!(
                    "Context edited · index={index} · entry={} · replacement={}",
                    entry_id,
                    replacement.as_deref().unwrap_or("<omitted>")
                ),
            ),
            HookFailed { hook, phase, error } => (
                Severity::Error,
                format!("Hook failed · {hook} · phase={phase}\n  error: {error}"),
            ),
            CompactionStart { input_bytes, automatic } => (
                Severity::Info,
                format!("Compaction started · input={input_bytes} bytes · automatic={automatic}"),
            ),
            CompactionEnd {
                entry_id,
                input_bytes,
                output_bytes,
                usage,
                automatic,
            } => {
                self.request_started = None;
                (
                    Severity::Info,
                    format!(
                        "Compaction finished · {input_bytes} → {output_bytes} bytes · automatic={automatic} · entry={entry_id} · {}",
                        tokens(usage)
                    ),
                )
            }
            CompactionFailed { error, automatic } => {
                self.request_started = None;
                (
                    Severity::Error,
                    format!("Compaction failed · automatic={automatic}\n  error: {error}"),
                )
            }
            ToolCallRecovered {
                tool_call_id,
                tool_name,
                state,
            } => (
                Severity::Warn,
                format!(
                    "Tool recovered · {tool_name} · call={tool_call_id} · {}",
                    state.describe()
                ),
            ),
            RunEnd { report } => self.run_finished(report),
        };
        Some(item)
    }

    /// A model request starts: it is counted and timed.
    fn request(&mut self, kind: RequestKind, entry_id: &str, model: &str, attempt: u32) -> (Severity, String) {
        self.requests = self.requests.saturating_add(1);
        self.request_started = Some(Instant::now());
        let turn = self.turn.map(|turn| format!(" · turn={turn}")).unwrap_or_default();
        (
            Severity::Info,
            format!(
                "Request #{} · {:?} · model={model}{turn} · attempt={attempt}{}",
                self.requests,
                kind,
                entry(entry_id)
            ),
        )
    }

    /// A response: how it ended, how long it took and what it used, its errors and reasoning.
    fn response(&mut self, message: &AssistantMessage) -> (Severity, String) {
        let elapsed = self
            .request_started
            .take()
            .map(|start| format!(" · elapsed={:.2}s", start.elapsed().as_secs_f64()))
            .unwrap_or_default();
        let mut text = format!(
            "Response #{} · {:?}{elapsed} · {}",
            self.requests,
            message.stop_reason,
            tokens(&message.usage)
        );
        if let Some(error) = &message.error_message {
            text.push_str(&format!("\n  error: {error}"));
            self.last_error = Some(error.clone());
        }
        if let Some(error) = &message.provider_error {
            text.push_str(&format!("\n  provider error: {error:?}"));
        }
        for block in &message.content {
            if let ContentBlock::Thinking { thinking, signature } = block {
                text.push_str(&format!("\n  reasoning: {thinking}"));
                if let Some(signature) = signature {
                    text.push_str(&format!("\n  reasoning signature: {signature}"));
                }
            }
        }
        let severity = if message.stop_reason == StopReason::Error {
            Severity::Error
        } else if message.stop_reason == StopReason::Aborted || message.stop_reason == StopReason::Length {
            Severity::Warn
        } else {
            Severity::Info
        };
        (severity, text)
    }

    /// The task ended: its report, with an error the timeline did not show yet.
    fn run_finished(&self, report: &RunReport) -> (Severity, String) {
        let mut text = format!(
            "Task finished · {:?} · model_requests={} · observed_requests={} · retries={} · {}",
            report.outcome,
            report.model_requests,
            self.requests,
            report.retries,
            tokens(&report.usage)
        );
        if let Some(path) = &report.session_file {
            text.push_str(&format!(" · session={path}"));
        }
        if let Some(error) = &report.error
            && self.last_error.as_deref() != Some(error)
        {
            text.push_str(&format!("\n  error: {error}"));
        }
        let severity = match report.outcome {
            RunOutcome::Error => Severity::Error,
            RunOutcome::Completed => Severity::Info,
            _ => Severity::Warn,
        };
        (severity, text)
    }

    /// ` · <error>`, unless it is the error reported just before.
    fn unless_last_error(&self, error: &str) -> String {
        if self.last_error.as_deref() == Some(error) {
            String::new()
        } else {
            format!(" · {error}")
        }
    }
}

/// A tool's result as the model sees it; a warning when it is an error.
fn tool_result(result: &ToolResultMessage, entry_id: &str) -> (Severity, String) {
    (
        if result.is_error {
            Severity::Warn
        } else {
            Severity::Info
        },
        format!(
            "Tool result · {} · call={}{}\n  {}",
            result.tool_name,
            result.tool_call_id,
            entry(entry_id),
            result.content
        ),
    )
}

/// The policy's verdict on a call; a warning unless it allows the call.
fn tool_decision(call: &str, tool: &str, verdict: ToolVerdict, reason: Option<&str>) -> (Severity, String) {
    (
        if matches!(verdict, ToolVerdict::Ask | ToolVerdict::Deny) {
            Severity::Warn
        } else {
            Severity::Info
        },
        format!(
            "Tool policy · {tool} · call={call} · {verdict:?}{}",
            reason_suffix(reason)
        ),
    )
}

/// How an approval ended and how long it waited; a warning unless the call may run.
fn approval_resolved(
    call: &str,
    tool: &str,
    approval: ApprovalResult,
    reason: Option<&str>,
    wait_ms: u64,
) -> (Severity, String) {
    (
        if matches!(
            approval,
            ApprovalResult::Denied | ApprovalResult::TimedOut | ApprovalResult::Cancelled | ApprovalResult::Failed
        ) {
            Severity::Warn
        } else {
            Severity::Info
        },
        format!(
            "Approval resolved · {tool} · call={call} · {approval:?} · {wait_ms}ms{}",
            reason_suffix(reason)
        ),
    )
}

/// A call finished, or never ran; a warning when it failed.
fn tool_finished(call: &str, tool: &str, is_error: bool, duration_ms: Option<u64>) -> (Severity, String) {
    (
        if is_error { Severity::Warn } else { Severity::Info },
        format!(
            "Tool finished · {tool} · call={call} · {} · {}",
            if is_error { "error" } else { "ok" },
            duration_ms.map_or_else(|| "not executed / duration unavailable".into(), |ms| format!("{ms}ms"))
        ),
    )
}

/// `<what> · entry=<id>`: a record was written.
fn recorded(what: &str, entry_id: &str) -> (Severity, String) {
    (Severity::Info, format!("{what} · entry={entry_id}"))
}

/// ` · entry=<id>`: the session entry the event names.
fn entry(entry_id: &str) -> String {
    format!(" · entry={entry_id}")
}

/// ` · <reason>` when there is one.
fn reason_suffix(reason: Option<&str>) -> String {
    reason.map(|reason| format!(" · {reason}")).unwrap_or_default()
}

fn tokens(usage: &Usage) -> String {
    format!(
        "tokens in={} out={} cache={} total={}",
        usage.input, usage.output, usage.cache_read, usage.total_tokens
    )
}

fn format_line(at_ms: u64, severity: Severity, text: &str, color: bool) -> String {
    let time = local_time(at_ms, TIMELINE_TIME_FORMAT).unwrap_or_else(|| format!("timestamp={at_ms}ms"));
    let (label, ansi) = match severity {
        Severity::Info => ("debug", "36"),
        Severity::Warn => ("warn", "33"),
        Severity::Error => ("error", "31"),
    };
    let prefix = format!("[{label} {time}]");
    let prefix = if color {
        format!("\x1b[{ansi}m{prefix}\x1b[0m")
    } else {
        prefix
    };
    format!("{prefix} {}", visible(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zx_agent::agent::RequestKind;
    use zx_agent::ai::AssistantMessage;

    fn event(run: &str, event: AgentEvent) -> Event {
        Event {
            schema_version: 1,
            seq: 1,
            at_ms: 0,
            session_id: None,
            run_id: run.into(),
            event,
        }
    }

    #[test]
    fn request_timeline_tracks_retries_and_resets_between_runs() {
        let mut state = DebugState::default();
        let request = |attempt| AgentEvent::RequestStart {
            kind: RequestKind::Task,
            entry_id: "r".into(),
            model: "model".into(),
            attempt,
        };
        let item = state.describe(&event("a", AgentEvent::TurnStart { turn: 1 }));
        assert!(item.is_none());
        assert!(
            state
                .describe(&event("a", request(1)))
                .unwrap()
                .1
                .contains("Request #1 · Task · model=model · turn=1 · attempt=1")
        );
        let message = AssistantMessage::failure("provider", "model", StopReason::Error, "failure marker");
        let text = state
            .describe(&event(
                "a",
                AgentEvent::MessageEnd {
                    message: Message::Assistant(message),
                    entry_id: "e".into(),
                    custom: None,
                },
            ))
            .unwrap()
            .1;
        assert!(text.contains("failure marker"));
        let text = state
            .describe(&event(
                "a",
                AgentEvent::RetryScheduled {
                    attempt: 1,
                    max_retries: 3,
                    delay_ms: 100,
                    error: "failure marker".into(),
                },
            ))
            .unwrap()
            .1;
        assert!(text.contains("Retry 1/3 in 100ms"));
        assert!(!text.contains("failure marker"));
        state.describe(&event("a", AgentEvent::TurnStart { turn: 2 }));
        assert!(
            state
                .describe(&event("a", request(2)))
                .unwrap()
                .1
                .contains("Request #2 · Task · model=model · turn=2 · attempt=2")
        );
        assert!(
            state
                .describe(&event("b", request(1)))
                .unwrap()
                .1
                .contains("Request #1")
        );
    }

    #[test]
    fn blocked_call_is_not_reported_as_executed() {
        let mut state = DebugState::default();
        let decision = AgentEvent::ToolDecision {
            tool_call_id: "id".into(),
            tool_name: "bash".into(),
            verdict: ToolVerdict::Deny,
            reason: Some("blocked".into()),
        };
        assert!(
            state
                .describe(&event("a", decision))
                .unwrap()
                .1
                .contains("Deny · blocked")
        );
        let end = AgentEvent::ToolExecutionEnd {
            tool_call_id: "id".into(),
            tool_name: "bash".into(),
            is_error: true,
            duration_ms: None,
            details: None,
            usage: None,
        };
        assert!(
            state
                .describe(&event("a", end))
                .unwrap()
                .1
                .contains("not executed / duration unavailable")
        );
    }
}
