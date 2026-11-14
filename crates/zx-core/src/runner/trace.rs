//! The trace of one run: what its hooks are told, the sequence that stamps its events, and
//! where those events go: to the run's reader, then to the hooks that observe them.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::ai::now_ms;
use crate::events::{EVENT_SCHEMA_VERSION, Emit, Event, EventKind};
use crate::guard::CancelSignal;
use crate::hooks::{HookChain, HookContext, HookError};
use crate::policy::PolicyContext;
use crate::sync::lock;

/// One run's context: what its hooks and policies are told and what stamps its events, with
/// the next sequence number.
pub(crate) struct RunTrace {
    session_id: String,
    run_id: String,
    cancel: CancellationToken,
    /// Replaced, not changed, when the run takes the input of another person: a call that is
    /// under way keeps what it was told.
    told: Mutex<Told>,
    seq: AtomicU64,
}

/// What the hooks and the policies of a run are told: the same run, as each kind sees it.
struct Told {
    hooks: Arc<HookContext>,
    policies: Arc<PolicyContext>,
}

impl Told {
    /// What `hooks` tells the hooks, and the same for the policies.
    fn of(hooks: HookContext) -> Self {
        let policies = PolicyContext {
            session_id: hooks.session_id.clone(),
            run_id: hooks.run_id.clone(),
            cwd: hooks.cwd.clone(),
            labels: hooks.labels.clone(),
            actor: hooks.actor.clone(),
            cancel: CancelSignal::new(hooks.cancel.token().clone()),
        };
        Self {
            hooks: Arc::new(hooks),
            policies: Arc::new(policies),
        }
    }
}

impl RunTrace {
    /// The run that `context` describes to its hooks: its session and its ID, the directory
    /// and the labels, whose input it answers at its start, and the loop's queues and tool
    /// set.
    pub(crate) fn new(context: HookContext) -> Self {
        Self {
            session_id: context.session_id.clone(),
            run_id: context.run_id.clone(),
            cancel: context.cancel.token().clone(),
            told: Mutex::new(Told::of(context)),
            seq: AtomicU64::new(0),
        }
    }

    /// What every hook of this run is told, as of now.
    pub(crate) fn context(&self) -> Arc<HookContext> {
        Arc::clone(&lock(&self.told).hooks)
    }

    /// What every policy of this run is told: the same run as the hooks'.
    pub(crate) fn policy_context(&self) -> Arc<PolicyContext> {
        Arc::clone(&lock(&self.told).policies)
    }

    /// The run takes an input that `actor` sent: its hooks and policies are told so from here
    /// on.
    pub(crate) fn answer(&self, actor: Option<&str>) {
        let mut told = lock(&self.told);
        if told.hooks.actor.as_deref() == actor {
            return;
        }
        let hooks = HookContext {
            actor: actor.map(String::from),
            ..HookContext::clone(&told.hooks)
        };
        *told = Told::of(hooks);
    }

    /// The run's cancellation.
    pub(crate) fn cancel(&self) -> &CancellationToken {
        &self.cancel
    }

    /// The event with the next sequence number. An event is stamped once it has its place
    /// among the events the reader takes, so the reader gets them in the order of their
    /// numbers.
    pub(crate) fn stamp(&self, event: EventKind) -> Event {
        Event {
            schema_version: EVENT_SCHEMA_VERSION,
            seq: self.seq.fetch_add(1, Ordering::Relaxed) + 1,
            at_ms: now_ms(),
            session_id: self.session_id.clone(),
            run_id: self.run_id.clone(),
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
        for error in self.hooks.on_event(&event, &self.trace.context()).await {
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
            drop(self.hooks.on_event(&report, &self.trace.context()).await);
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
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;

    fn trace() -> RunTrace {
        RunTrace::new(HookContext::new(
            "session".into(),
            PathBuf::from("."),
            CancellationToken::new(),
        ))
    }

    #[test]
    fn hooks_and_policies_are_told_whose_input_the_run_answers_from_then_on() {
        let trace = trace();
        let before = trace.context();
        assert_eq!(
            (before.actor.as_deref(), trace.policy_context().actor.as_deref()),
            (None, None)
        );

        trace.answer(Some("alice"));

        assert_eq!(before.actor, None, "a call under way keeps what it was told");
        let (hooks, policies) = (trace.context(), trace.policy_context());
        assert_eq!(
            (hooks.actor.as_deref(), policies.actor.as_deref()),
            (Some("alice"), Some("alice"))
        );
        assert_eq!(
            (hooks.run_id.as_str(), policies.run_id.as_str()),
            (before.run_id.as_str(), before.run_id.as_str())
        );
        trace.answer(None);
        assert_eq!(trace.context().actor, None);
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
