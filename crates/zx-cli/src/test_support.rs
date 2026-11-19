//! What the unit tests of several modules share.

use std::collections::BTreeMap;

use zx_agent::ai::Usage;
use zx_agent::events::{RunOutcome, RunReport};
use zx_agent::transcript::SessionInfo;

/// The report of a run that ended with `outcome` after one model request, without text,
/// usage or an error.
pub(crate) fn report(outcome: RunOutcome) -> RunReport {
    RunReport {
        outcome,
        final_text: String::new(),
        model_requests: 1,
        retries: 0,
        usage: Usage::default(),
        tool_usage: BTreeMap::new(),
        denied_calls: 0,
        error: None,
        session_id: "session".into(),
    }
}

/// A listed session `id`, with nothing else: no directory, times, messages or name.
pub(crate) fn session(id: &str) -> SessionInfo {
    SessionInfo {
        id: id.into(),
        cwd: String::new(),
        started_ms: 0,
        last_activity_ms: 0,
        messages: 0,
        first_prompt: None,
        name: None,
        labels: BTreeMap::new(),
    }
}
