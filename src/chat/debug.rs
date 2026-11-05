//! Human-readable execution timeline for chat's text debug mode.

use std::io::IsTerminal;
use std::time::Instant;

use chrono::{DateTime, Local};
use zx_agent::agent::{AgentEvent, ApprovalResult, Event, RunOutcome, ToolVerdict};
use zx_agent::ai::{ContentBlock, Message, StopReason};

use super::output::visible;

#[derive(Default)]
pub(super) struct DebugState {
    run_id: String,
    turn: Option<u32>,
    requests: u64,
    request_started: Option<Instant>,
    last_error: Option<String>,
}

impl DebugState {
    pub(super) fn emit(&mut self, event: &Event) {
        let Some((severity, text)) = self.describe(event) else {
            return;
        };
        let color = std::io::stderr().is_terminal()
            && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
            && std::env::var_os("TERM").is_none_or(|value| value != "dumb");
        eprintln!("{}", format_line(event.at_ms, severity, &text, color));
    }

    fn describe(&mut self, event: &Event) -> Option<(Severity, String)> {
        if self.run_id != event.run_id {
            *self = Self {
                run_id: event.run_id.clone(),
                ..Self::default()
            };
        }
        use AgentEvent::*;
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
            } => {
                self.requests = self.requests.saturating_add(1);
                self.request_started = Some(Instant::now());
                let turn = self.turn.map(|turn| format!(" · turn={turn}")).unwrap_or_default();
                let entry = entry_id
                    .as_deref()
                    .map(|id| format!(" · entry={id}"))
                    .unwrap_or_default();
                (
                    Severity::Info,
                    format!(
                        "Request #{} · {:?} · model={model}{turn} · attempt={attempt}{entry}",
                        self.requests, kind
                    ),
                )
            }
            MessageEnd {
                message: Message::Assistant(message),
                ..
            } => {
                let elapsed = self
                    .request_started
                    .take()
                    .map(|start| format!(" · elapsed={:.2}s", start.elapsed().as_secs_f64()))
                    .unwrap_or_default();
                let usage = &message.usage;
                let mut text = format!(
                    "Response #{} · {:?}{elapsed} · tokens in={} out={} cache={} total={}",
                    self.requests, message.stop_reason, usage.input, usage.output, usage.cache_read, usage.total_tokens
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
            MessageEnd {
                message: Message::ToolResult(result),
                entry_id,
            } => {
                let entry = entry_id
                    .as_deref()
                    .map(|id| format!(" · entry={id}"))
                    .unwrap_or_default();
                (
                    if result.is_error {
                        Severity::Warn
                    } else {
                        Severity::Info
                    },
                    format!(
                        "Tool result · {} · call={}{entry}\n  {}",
                        result.tool_name, result.tool_call_id, result.content
                    ),
                )
            }
            MessageEnd {
                message: Message::User { kind, .. },
                entry_id,
            } => (
                Severity::Info,
                format!(
                    "User input recorded · kind={kind:?} · entry={}",
                    entry_id.as_deref().unwrap_or("none")
                ),
            ),
            MessageEnd {
                message: Message::System(_),
                entry_id,
            } => (
                Severity::Info,
                format!(
                    "System prompt changes recorded · entry={}",
                    entry_id.as_deref().unwrap_or("none")
                ),
            ),
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
            } => (
                if matches!(verdict, ToolVerdict::Ask | ToolVerdict::Deny) {
                    Severity::Warn
                } else {
                    Severity::Info
                },
                format!(
                    "Tool policy · {tool_name} · call={tool_call_id} · {verdict:?}{}",
                    reason.as_deref().map(|r| format!(" · {r}")).unwrap_or_default()
                ),
            ),
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
            } => (
                if matches!(
                    approval,
                    ApprovalResult::Denied
                        | ApprovalResult::TimedOut
                        | ApprovalResult::Cancelled
                        | ApprovalResult::Failed
                ) {
                    Severity::Warn
                } else {
                    Severity::Info
                },
                format!(
                    "Approval resolved · {tool_name} · call={tool_call_id} · {approval:?} · {wait_ms}ms{}",
                    reason.as_deref().map(|r| format!(" · {r}")).unwrap_or_default()
                ),
            ),
            ToolExecutionEnd {
                tool_call_id,
                tool_name,
                is_error,
                duration_ms,
                details: _,
            } => (
                if *is_error { Severity::Warn } else { Severity::Info },
                format!(
                    "Tool finished · {tool_name} · call={tool_call_id} · {} · {}",
                    if *is_error { "error" } else { "ok" },
                    duration_ms
                        .map(|ms| format!("{ms}ms"))
                        .unwrap_or_else(|| "not executed / duration unavailable".into())
                ),
            ),
            RetryScheduled {
                attempt,
                max_retries,
                delay_ms,
                error,
            } => {
                let extra = if self.last_error.as_deref() == Some(error) {
                    String::new()
                } else {
                    format!(" · {error}")
                };
                (
                    Severity::Warn,
                    format!("Retry {attempt}/{max_retries} in {delay_ms}ms{extra}"),
                )
            }
            ContextOverflow { error } => {
                let extra = if self.last_error.as_deref() == Some(error) {
                    String::new()
                } else {
                    format!(" · {error}")
                };
                (
                    Severity::Warn,
                    format!("Context too long for the model; compacting before another attempt{extra}"),
                )
            }
            ContextEdited {
                index,
                replacement,
                entry_id,
            } => (
                Severity::Info,
                format!(
                    "Context edited · index={index} · entry={} · replacement={}",
                    entry_id.as_deref().unwrap_or("none"),
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
                        "Compaction finished · {input_bytes} → {output_bytes} bytes · automatic={automatic} · entry={entry_id} · tokens in={} out={} cache={} total={}",
                        usage.input, usage.output, usage.cache_read, usage.total_tokens
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
            RunEnd { report } => {
                let usage = &report.usage;
                let mut text = format!(
                    "Task finished · {:?} · model_requests={} · observed_requests={} · retries={} · tokens in={} out={} cache={} total={}",
                    report.outcome,
                    report.model_requests,
                    self.requests,
                    report.retries,
                    usage.input,
                    usage.output,
                    usage.cache_read,
                    usage.total_tokens
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
        };
        Some(item)
    }
}

#[derive(Clone, Copy)]
enum Severity {
    Info,
    Warn,
    Error,
}

fn format_line(at_ms: u64, severity: Severity, text: &str, color: bool) -> String {
    let time = i64::try_from(at_ms)
        .ok()
        .and_then(DateTime::from_timestamp_millis)
        .map(|utc| {
            utc.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S%.3f %:z")
                .to_string()
        })
        .unwrap_or_else(|| format!("timestamp={at_ms}ms"));
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
            entry_id: None,
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
                    entry_id: None,
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
