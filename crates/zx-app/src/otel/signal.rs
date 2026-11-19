//! Events as signals: what the exporter task needs from each event, metadata only, handed
//! over by a hook through a bounded queue that a run never waits for.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use opentelemetry::trace::SpanContext;
use tokio::sync::mpsc;

use zx_agent::ai::{Message, StopReason, Usage};
use zx_agent::events::{ApprovalResult, Event, EventKind, Interruption, RequestKind, RunOutcome};
use zx_agent::hooks::{Hook, HookContext, HookResult};

/// Observes the runs of the agent it is registered with, and passes the events that spans
/// need to the exporter task. It never waits and never fails, so it sees every event in
/// order, also those of a cancelled run. Made by [`Telemetry::hook`](super::Telemetry::hook);
/// its copies export to the same place.
#[derive(Clone)]
pub struct TraceHook {
    pub(super) sender: mpsc::Sender<Signal>,
    pub(super) dropped: Arc<AtomicU64>,
    /// The caller's span, which the runs continue.
    pub(super) parent: Option<SpanContext>,
}

impl TraceHook {
    fn observe(&self, event: &Event) {
        let Some(signal) = Signal::of(event, self.parent.as_ref()) else {
            return;
        };
        if self.sender.try_send(signal).is_err() {
            // Full, or the export has stopped: a run never waits for telemetry.
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[async_trait]
impl Hook for TraceHook {
    async fn on_event(&self, event: &Event, _ctx: &HookContext) -> HookResult<()> {
        self.observe(event);
        Ok(())
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
    StepStart {
        step: u32,
    },
    StepEnd,
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
        /// The policies' decision, without its reason, which may quote the call.
        decision: &'static str,
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
            EventKind::RunStart { model, provider } => What::RunStart {
                model: model.clone(),
                provider: provider.clone(),
                parent: parent.cloned(),
            },
            EventKind::RunEnd { report } => What::RunEnd {
                outcome: report.outcome,
                model_requests: report.model_requests,
                retries: report.retries,
                usage: report.usage.clone(),
            },
            EventKind::AttemptStart { attempt } => What::AttemptStart { attempt: *attempt },
            EventKind::AttemptEnd { outcome } => What::AttemptEnd { outcome: *outcome },
            EventKind::StepStart { step } => What::StepStart { step: *step },
            EventKind::StepEnd { .. } => What::StepEnd,
            EventKind::RequestStart {
                kind, model, attempt, ..
            } => What::RequestStart {
                kind: *kind,
                model: model.clone(),
                attempt: *attempt,
            },
            EventKind::MessageEnd {
                message: Message::Assistant(message),
                ..
            } => What::Response {
                usage: message.usage.clone(),
                stop: message.stop_reason,
            },
            EventKind::ToolExecutionStart {
                tool_call_id,
                tool_name,
                ..
            } => What::ToolStart {
                call: tool_call_id.clone(),
                tool: tool_name.clone(),
            },
            EventKind::ToolDecision {
                tool_call_id, decision, ..
            } => What::Decision {
                call: tool_call_id.clone(),
                decision: decision.name(),
            },
            EventKind::ApprovalRequested { tool_call_id, .. } => What::ApprovalRequested {
                call: tool_call_id.clone(),
            },
            EventKind::ApprovalResolved {
                tool_call_id,
                approval,
                wait_ms,
                ..
            } => What::ApprovalResolved {
                call: tool_call_id.clone(),
                result: *approval,
                wait_ms: *wait_ms,
            },
            EventKind::ToolExecutionEnd {
                tool_call_id,
                is_error,
                usage,
                ..
            } => What::ToolEnd {
                call: tool_call_id.clone(),
                is_error: *is_error,
                usage: usage.clone(),
            },
            EventKind::CompactionStart { input_bytes, automatic } => What::CompactionStart {
                automatic: *automatic,
                input_bytes: *input_bytes,
                parent: parent.cloned(),
            },
            EventKind::CompactionEnd {
                usage, output_bytes, ..
            } => What::CompactionEnd {
                usage: usage.clone(),
                output_bytes: *output_bytes,
            },
            EventKind::CompactionFailed { .. } => What::CompactionFailed,
            EventKind::RetryScheduled { retry, delay_ms, .. } => What::Retry {
                retry: *retry,
                delay_ms: *delay_ms,
            },
            // The error text may quote the request, so only the fact is exported.
            EventKind::ContextOverflow { .. } => What::Overflow,
            EventKind::HookFailed { hook, phase, .. } => What::HookFailed {
                hook: hook.clone(),
                phase: phase.name(),
            },
            EventKind::ToolCallRecovered {
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
            EventKind::MessageStart { .. }
            | EventKind::MessageUpdate { .. }
            | EventKind::MessageEnd { .. }
            | EventKind::ToolExecutionUpdate { .. }
            | EventKind::ContextEdited { .. }
            | EventKind::CustomEntry { .. } => return None,
        };
        Some(Self {
            run_id: event.run_id.clone(),
            session_id: event.session_id.clone(),
            at: UNIX_EPOCH + Duration::from_millis(event.at_ms),
            what,
        })
    }
}

#[cfg(test)]
mod tests {
    use zx_agent::ai::Role;

    use super::*;
    use crate::otel::test_support::event;

    #[test]
    fn the_hook_drops_events_beyond_its_queue_and_counts_them() {
        let (sender, _receiver) = mpsc::channel(1);
        let hook = TraceHook {
            sender,
            dropped: Arc::new(AtomicU64::new(0)),
            parent: None,
        };
        hook.observe(&event(1, EventKind::StepStart { step: 1 }));
        hook.observe(&event(2, EventKind::StepStart { step: 2 }));
        // Not for spans: neither queued nor counted.
        hook.observe(&event(3, EventKind::MessageStart { role: Role::Assistant }));
        assert_eq!(hook.dropped.load(Ordering::Relaxed), 1);
    }
}
