//! Approval of a tool call that a policy asks about: who answers, what the answer is, and
//! the gate where the call waits for it and the session keeps what it was granted.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::ai::ToolCall;
use crate::sync::lock;

/// Why a call is denied when nobody answers approvals ([`ApprovalMode::Deny`]).
pub(crate) const NO_HUMAN: &str = "no human is available to approve this call";

/// Who answers when a policy asks about a tool call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ApprovalMode {
    /// Nobody: the call is denied at once. The default, for runs without a person.
    #[default]
    Deny,
    /// The host: the run emits `approval_requested` with the request's ID and waits this
    /// long for [`Session::resolve_approval`](crate::Session::resolve_approval). A request
    /// that nobody answers in time, or whose run is cancelled, is denied.
    Wait(Duration),
}

/// A host's answer to an approval request. A session grant covers only the exact tool name
/// and arguments. In JSON: `{"approval":"allow_once"}`, `allow_session`, or `deny` with a
/// `reason`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "approval", content = "reason", rename_all = "snake_case")]
pub enum ApprovalDecision {
    AllowOnce,
    AllowSession,
    Deny(String),
}

/// The approvals of one session: the requests that wait for the host's answer, and the
/// calls the host allowed for the rest of the session. In memory only, never recorded; the
/// session's handles and its run share it, so a host answers from whatever task it is in.
#[derive(Default)]
pub(crate) struct ApprovalGate {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// The requests that wait, by ID, with where the answer of each goes.
    waiting: HashMap<String, oneshot::Sender<ApprovalDecision>>,
    /// The calls allowed for the session: exact tool names and arguments.
    granted: Vec<(String, Value)>,
}

impl ApprovalGate {
    /// Whether the host allowed this exact call for the session.
    pub(crate) fn allows(&self, call: &ToolCall) -> bool {
        granted(&lock(&self.state).granted, call)
    }

    /// Allow this exact call for the rest of the session.
    pub(crate) fn remember(&self, call: &ToolCall) {
        let mut state = lock(&self.state);
        if !granted(&state.granted, call) {
            state.granted.push((call.name.clone(), call.arguments.clone()));
        }
    }

    /// Open a request for the host to answer, with an ID of its own ([`request_id`]).
    pub(crate) fn open(&self) -> Waiting<'_> {
        let id = request_id();
        let (decide, answer) = oneshot::channel();
        lock(&self.state).waiting.insert(id.clone(), decide);
        Waiting { gate: self, id, answer }
    }

    /// Give `decision` to the request `id`. False when no such request waits: it was
    /// answered already, it timed out, or its run was cancelled.
    pub(crate) fn resolve(&self, id: &str, decision: ApprovalDecision) -> bool {
        let Some(decide) = lock(&self.state).waiting.remove(id) else {
            return false;
        };
        // The request may stop waiting just now; then nobody takes the answer.
        decide.send(decision).is_ok()
    }
}

/// A new ID for an approval request: the model chooses the IDs of its calls and may repeat
/// one, and a late answer must not reach another request.
pub(crate) fn request_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn granted(calls: &[(String, Value)], call: &ToolCall) -> bool {
    calls
        .iter()
        .any(|(name, arguments)| name == &call.name && arguments == &call.arguments)
}

/// How a request ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Answer {
    Decided(ApprovalDecision),
    /// Nobody answered within this time.
    TimedOut(Duration),
    /// The run was cancelled while the request waited.
    Cancelled,
}

/// A request that waits for the host's answer. Dropping it withdraws the request, so an
/// answer that comes later finds none.
pub(crate) struct Waiting<'a> {
    gate: &'a ApprovalGate,
    id: String,
    answer: oneshot::Receiver<ApprovalDecision>,
}

impl Waiting<'_> {
    /// The request's ID, which the host answers with.
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    /// Wait for the host's answer, at most `timeout` and until `cancel` is cancelled.
    pub(crate) async fn answer(&mut self, timeout: Duration, cancel: &CancellationToken) -> Answer {
        tokio::select! {
            biased;
            () = cancel.cancelled() => Answer::Cancelled,
            decision = &mut self.answer => match decision {
                Ok(decision) => Answer::Decided(decision),
                // Only this request's own withdrawal drops the sender unanswered.
                Err(_) => Answer::Cancelled,
            },
            () = tokio::time::sleep(timeout) => Answer::TimedOut(timeout),
        }
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        lock(&self.gate.state).waiting.remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const LONG: Duration = Duration::from_secs(60);

    fn call(arguments: Value) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: "bash".into(),
            arguments,
            arguments_error: None,
        }
    }

    #[tokio::test]
    async fn an_answer_reaches_the_request_it_names_once() {
        let gate = ApprovalGate::default();
        let mut request = gate.open();
        let id = request.id().to_string();

        assert!(!gate.resolve("another", ApprovalDecision::AllowOnce));
        assert!(gate.resolve(&id, ApprovalDecision::Deny("not now".into())));
        assert!(!gate.resolve(&id, ApprovalDecision::AllowOnce), "answered already");

        let answer = request.answer(LONG, &CancellationToken::new()).await;
        assert_eq!(answer, Answer::Decided(ApprovalDecision::Deny("not now".into())));
    }

    #[tokio::test]
    async fn a_request_ends_by_its_timeout_or_the_runs_cancellation() {
        let gate = ApprovalGate::default();
        let brief = Duration::from_millis(1);
        let mut request = gate.open();
        assert_eq!(
            request.answer(brief, &CancellationToken::new()).await,
            Answer::TimedOut(brief)
        );

        let cancel = CancellationToken::new();
        cancel.cancel();
        // Cancellation counts even when an answer has arrived as well.
        assert!(gate.resolve(request.id(), ApprovalDecision::AllowOnce));
        assert_eq!(request.answer(LONG, &cancel).await, Answer::Cancelled);
    }

    #[test]
    fn an_answer_after_the_request_was_withdrawn_finds_none() {
        let gate = ApprovalGate::default();
        let request = gate.open();
        let id = request.id().to_string();

        drop(request);

        assert!(!gate.resolve(&id, ApprovalDecision::AllowOnce));
    }

    #[test]
    fn a_session_grant_covers_the_exact_call_only() {
        let gate = ApprovalGate::default();
        let allowed = call(json!({"command": "ls"}));
        assert!(!gate.allows(&allowed));

        gate.remember(&allowed);
        gate.remember(&allowed);

        assert!(gate.allows(&allowed));
        assert!(!gate.allows(&call(json!({"command": "ls -l"}))));
        assert!(!gate.allows(&ToolCall {
            name: "other".into(),
            ..allowed.clone()
        }));
    }
}
