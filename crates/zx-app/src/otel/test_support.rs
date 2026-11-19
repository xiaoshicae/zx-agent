//! What the tests of the signals and of the spans share.

use zx_agent::events::{EVENT_SCHEMA_VERSION, Event, EventKind};

/// The run of the tests' events.
pub(super) const RUN: &str = "6f1c2c4e-0000-4000-8000-000000000001";

/// An event of the run `RUN` at `at_ms`, in session `s1`.
pub(super) fn event(at_ms: u64, event: EventKind) -> Event {
    Event {
        schema_version: EVENT_SCHEMA_VERSION,
        seq: at_ms,
        at_ms,
        session_id: "s1".into(),
        run_id: RUN.into(),
        event,
    }
}
