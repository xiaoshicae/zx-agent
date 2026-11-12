//! Events as signals: what the exporter task needs from each event, metadata only, handed
//! over through a bounded queue that a run never waits for.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use opentelemetry::trace::SpanContext;
use tokio::sync::mpsc;

use crate::agent::{AgentEvent, ApprovalResult, Event, EventSink, Interruption, RequestKind, RunOutcome, ToolVerdict};
use crate::ai::{Message, StopReason, Usage};

/// Passes the events that spans need to the exporter task.
pub(super) struct SignalSink {
    pub(super) sender: mpsc::Sender<Signal>,
    pub(super) dropped: Arc<AtomicU64>,
    /// The caller's span, which the runs continue.
    pub(super) parent: Option<SpanContext>,
}

impl EventSink for SignalSink {
    fn emit(&self, event: &Event) {
        let Some(signal) = Signal::of(event, self.parent.as_ref()) else {
            return;
        };
        if self.sender.try_send(signal).is_err() {
            // Full, or the export has stopped: a run never waits for telemetry.
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// What a span needs from one event: metadata only.
pub(super) struct Signal {
    pub(super) run_id: String,
    pub(super) session_id: String,
    pub(super) at: SystemTime,
    pub(super) what: What,
}

pub(super) enum What {
    RunStart {
        model: String,
        provider: String,
        /// The caller's span to start the trace under: only a trace's first span has a
        /// parent outside the trace.
        parent: Option<SpanContext>,
    },
    RunEnd {
        outcome: RunOutcome,
        model_requests: u32,
        retries: u32,
        usage: Usage,
    },
    AttemptStart {
        attempt: u32,
    },
    AttemptEnd {
        outcome: RunOutcome,
    },
    TurnStart {
        turn: u32,
    },
    TurnEnd,
    RequestStart {
        kind: RequestKind,
        model: String,
        attempt: u32,
    },
    Response {
        usage: Usage,
        stop: StopReason,
    },
    ToolStart {
        call: String,
        tool: String,
    },
    Decision {
        call: String,
        verdict: ToolVerdict,
    },
    ApprovalRequested {
        call: String,
    },
    ApprovalResolved {
        call: String,
        result: ApprovalResult,
        wait_ms: u64,
    },
    ToolEnd {
        call: String,
        is_error: bool,
        /// What the tool spent on work of its own, when it reported some.
        usage: Option<Usage>,
    },
    CompactionStart {
        automatic: bool,
        input_bytes: usize,
        /// The caller's span, for a manual compaction, which belongs to no run and so starts
        /// a trace of its own.
        parent: Option<SpanContext>,
    },
    CompactionEnd {
        usage: Usage,
        output_bytes: usize,
    },
    CompactionFailed,
    Retry {
        retry: u32,
        delay_ms: u64,
    },
    Overflow,
    HookFailed {
        hook: String,
        phase: &'static str,
    },
    Recovered {
        call: String,
        tool: String,
        state: Interruption,
    },
}

impl Signal {
    #[expect(clippy::too_many_lines, reason = "one arm per event variant")]
    pub(super) fn of(event: &Event, parent: Option<&SpanContext>) -> Option<Self> {
        let what = match &event.event {
            AgentEvent::RunStart { model, provider } => What::RunStart {
                model: model.clone(),
                provider: provider.clone(),
                parent: parent.cloned(),
            },
            AgentEvent::RunEnd { report } => What::RunEnd {
                outcome: report.outcome,
                model_requests: report.model_requests,
                retries: report.retries,
                usage: report.usage.clone(),
            },
            AgentEvent::AgentStart { attempt } => What::AttemptStart { attempt: *attempt },
            AgentEvent::AgentEnd { outcome } => What::AttemptEnd { outcome: *outcome },
            AgentEvent::TurnStart { turn } => What::TurnStart { turn: *turn },
            AgentEvent::TurnEnd { .. } => What::TurnEnd,
            AgentEvent::RequestStart {
                kind, model, attempt, ..
            } => What::RequestStart {
                kind: *kind,
                model: model.clone(),
                attempt: *attempt,
            },
            AgentEvent::MessageEnd {
                message: Message::Assistant(message),
                ..
            } => What::Response {
                usage: message.usage.clone(),
                stop: message.stop_reason,
            },
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                ..
            } => What::ToolStart {
                call: tool_call_id.clone(),
                tool: tool_name.clone(),
            },
            AgentEvent::ToolDecision {
                tool_call_id, verdict, ..
            } => What::Decision {
                call: tool_call_id.clone(),
                verdict: *verdict,
            },
            AgentEvent::ApprovalRequested { tool_call_id, .. } => What::ApprovalRequested {
                call: tool_call_id.clone(),
            },
            AgentEvent::ApprovalResolved {
                tool_call_id,
                approval,
                wait_ms,
                ..
            } => What::ApprovalResolved {
                call: tool_call_id.clone(),
                result: *approval,
                wait_ms: *wait_ms,
            },
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                is_error,
                usage,
                ..
            } => What::ToolEnd {
                call: tool_call_id.clone(),
                is_error: *is_error,
                usage: usage.clone(),
            },
            AgentEvent::CompactionStart { input_bytes, automatic } => What::CompactionStart {
                automatic: *automatic,
                input_bytes: *input_bytes,
                parent: parent.cloned(),
            },
            AgentEvent::CompactionEnd {
                usage, output_bytes, ..
            } => What::CompactionEnd {
                usage: usage.clone(),
                output_bytes: *output_bytes,
            },
            AgentEvent::CompactionFailed { .. } => What::CompactionFailed,
            AgentEvent::RetryScheduled { retry, delay_ms, .. } => What::Retry {
                retry: *retry,
                delay_ms: *delay_ms,
            },
            // The error text may quote the request, so only the fact is exported.
            AgentEvent::ContextOverflow { .. } => What::Overflow,
            AgentEvent::HookFailed { hook, phase, .. } => What::HookFailed {
                hook: hook.clone(),
                phase: phase.as_str(),
            },
            AgentEvent::ToolCallRecovered {
                tool_call_id,
                tool_name,
                state,
            } => What::Recovered {
                call: tool_call_id.clone(),
                tool: tool_name.clone(),
                state: *state,
            },
            // Streaming, the other messages, context edits and extension records are
            // content, not spans.
            AgentEvent::MessageStart { .. }
            | AgentEvent::MessageUpdate { .. }
            | AgentEvent::MessageEnd { .. }
            | AgentEvent::ToolExecutionUpdate { .. }
            | AgentEvent::ContextEdited { .. }
            | AgentEvent::CustomEntry { .. } => return None,
        };
        Some(Self {
            run_id: event.run_id.clone(),
            session_id: event.session_id.clone(),
            at: UNIX_EPOCH + Duration::from_millis(event.at_ms),
            what,
        })
    }
}

/// An event of the run the tests use, at `at_ms`, in session `s1`.
#[cfg(test)]
pub(super) fn event(at_ms: u64, event: AgentEvent) -> Event {
    Event {
        schema_version: crate::agent::EVENT_SCHEMA_VERSION,
        seq: at_ms,
        at_ms,
        session_id: "s1".into(),
        run_id: "6f1c2c4e-0000-4000-8000-000000000001".into(),
        event,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sink_drops_events_beyond_its_queue_and_counts_them() {
        let (sender, _receiver) = mpsc::channel(1);
        let sink = SignalSink {
            sender,
            dropped: Arc::new(AtomicU64::new(0)),
            parent: None,
        };
        sink.emit(&event(1, AgentEvent::TurnStart { turn: 1 }));
        sink.emit(&event(2, AgentEvent::TurnStart { turn: 2 }));
        // Not for spans: neither queued nor counted.
        sink.emit(&event(3, AgentEvent::MessageStart { role: "assistant" }));
        assert_eq!(sink.dropped.load(Ordering::Relaxed), 1);
    }
}
