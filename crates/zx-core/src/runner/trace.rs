//! The trace of one run: what its hooks are told, the sequence that stamps its events, and
//! where those events go: to the run's reader, then to the hooks that observe them.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::ai::now_ms;
use crate::events::{EVENT_SCHEMA_VERSION, Emit, Event, EventKind};
use crate::guard::CancelSignal;
use crate::hooks::{HookChain, HookContext, HookError};
use crate::input::InputQueues;
use crate::policy::PolicyContext;
use crate::tools::ActiveTools;

/// One run's context: what its hooks and policies are told and what stamps its events, with
/// the next sequence number.
pub(crate) struct RunTrace {
    context: HookContext,
    policy_context: PolicyContext,
    seq: AtomicU64,
}

impl RunTrace {
    /// The run `run_id` of session `session_id`, cancelled by `cancel`, whose hooks reach the
    /// loop's queues and tool set.
    pub(crate) fn new(
        (session_id, run_id): (String, String),
        cwd: PathBuf,
        cancel: CancellationToken,
        queues: Arc<InputQueues>,
        active_tools: Arc<ActiveTools>,
    ) -> Self {
        let context = HookContext {
            run_id,
            ..HookContext::new(session_id, cwd, cancel).with_shared(queues, active_tools)
        };
        let policy_context = PolicyContext {
            session_id: context.session_id.clone(),
            run_id: context.run_id.clone(),
            cwd: context.cwd.clone(),
            cancel: CancelSignal::new(context.cancel.token().clone()),
        };
        Self {
            context,
            policy_context,
            seq: AtomicU64::new(0),
        }
    }

    /// What every hook of this run is told.
    pub(crate) fn context(&self) -> &HookContext {
        &self.context
    }

    /// What every policy of this run is told: the same run as the hooks'.
    pub(crate) fn policy_context(&self) -> &PolicyContext {
        &self.policy_context
    }

    /// The run's cancellation.
    pub(crate) fn cancel(&self) -> &CancellationToken {
        self.context.cancel.token()
    }

    /// The event with the next sequence number. An event is stamped once it has its place
    /// among the events the reader takes, so the reader gets them in the order of their
    /// numbers.
    pub(crate) fn stamp(&self, event: EventKind) -> Event {
        Event {
            schema_version: EVENT_SCHEMA_VERSION,
            seq: self.seq.fetch_add(1, Ordering::Relaxed) + 1,
            at_ms: now_ms(),
            session_id: self.context.session_id.clone(),
            run_id: self.context.run_id.clone(),
            event,
        }
    }
}

/// Where the events of a run go: stamped by `trace`, queued for the run's reader in
/// `events`, then handed to the `hooks` that observe them.
#[derive(Clone, Copy)]
pub(crate) struct EventTarget<'a> {
    pub(crate) events: &'a mpsc::Sender<Event>,
    pub(crate) trace: &'a RunTrace,
    pub(crate) hooks: &'a HookChain,
}

impl EventTarget<'_> {
    /// Stamp `event`, queue it for the reader, then hand it to the hooks that observe it. A
    /// hook that fails to observe it is reported and does not stop the run. Waits while the
    /// reader is as far behind as the run may get ahead.
    pub(crate) async fn emit(self, event: EventKind) {
        // What the event reports is already recorded; it is queued for the reader before
        // observers see it, so the report of an observer's failure follows it.
        let Some(event) = self.queue(event).await else {
            return;
        };
        for error in self.hooks.on_event(&event, self.trace.context()).await {
            self.report_failure(error).await;
        }
    }

    /// Stamp `event` and queue it for the reader. Returns a copy for the hooks, when there
    /// are any to observe it.
    async fn queue(self, event: EventKind) -> Option<Event> {
        // The place first, then the number: tools that emit at the same time still reach
        // the reader in the order of their numbers.
        let place = self.events.reserve().await;
        let event = self.trace.stamp(event);
        let observed = (!self.hooks.is_empty()).then(|| event.clone());
        // Without a place the reader is gone, and with it the run: nobody takes its events
        // any more.
        if let Ok(place) = place {
            place.send(event);
        }
        observed
    }

    /// Report a hook failure as a `hook_failed` event, which the hooks observe like any
    /// other. Returns its description.
    pub(crate) async fn report_failure(self, error: HookError) -> String {
        let text = error.to_string();
        let report = EventKind::HookFailed {
            hook: error.hook,
            phase: error.phase,
            error: text.clone(),
        };
        if let Some(report) = self.queue(report).await {
            // A hook that fails to observe the report is not reported in turn: it would
            // fail on that report as well, without end.
            drop(self.hooks.on_event(&report, self.trace.context()).await);
        }
        text
    }
}

#[async_trait]
impl Emit for EventTarget<'_> {
    async fn emit(&self, event: EventKind) {
        // The inherent method, which takes the target by value.
        EventTarget::emit(*self, event).await;
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn trace() -> RunTrace {
        RunTrace::new(
            ("session".into(), uuid::Uuid::new_v4().to_string()),
            PathBuf::from("."),
            CancellationToken::new(),
            Arc::default(),
            Arc::default(),
        )
    }

    #[test]
    fn the_envelope_shares_one_object_with_the_event() {
        let trace = trace();
        let first = trace.stamp(EventKind::AttemptStart { attempt: 1 });
        let second = trace.stamp(EventKind::StepStart { step: 1 });

        let value = serde_json::to_value(&second).unwrap();

        assert_eq!(value["schemaVersion"], EVENT_SCHEMA_VERSION);
        assert_eq!((first.seq, value["seq"].clone()), (1, json!(2)));
        assert_eq!(value["sessionId"], "session");
        assert_eq!(value["runId"], first.run_id.as_str());
        assert_eq!(
            (value["type"].clone(), value["step"].clone()),
            (json!("step_start"), json!(1))
        );
        assert!(value["atMs"].as_u64().is_some_and(|ms| ms >= first.at_ms));
        assert!(value.get("event").is_none(), "the event is not nested");
    }

    #[test]
    fn each_run_has_its_own_id_and_numbering() {
        let first = trace();
        let second = trace();
        first.stamp(EventKind::AttemptStart { attempt: 1 });

        let event = second.stamp(EventKind::AttemptStart { attempt: 1 });

        assert_eq!(event.seq, 1);
        assert_ne!(event.run_id, first.stamp(EventKind::AttemptStart { attempt: 1 }).run_id);
    }
}
