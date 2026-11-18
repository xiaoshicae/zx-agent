//! Approvals in the chat: a card with buttons, answered by an approver's click.
//!
//! The task that reads a run passes its approval events on to the worker
//! ([`ApprovalEvents`]): the worker posts a card for each request, answers the request when
//! an approver clicks, and closes the card when the run reports how the approval ended.

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc;

use zx_core::events::{ApprovalResult, Event, EventKind};
use zx_util::text::clip;

use super::{ApprovalCard, ApprovalId, ServerEvent, ServerSink, SessionKey};

/// Longest command or argument text shown on a card; longer text ends with "...". A card is
/// for deciding at a glance; the full call is in the session file.
const MAX_SUMMARY_CHARS: usize = 1500;

/// What the worker learns about approvals, in order: a call waits for one, then the run
/// reported how it ended. Both travel through one channel, so the request is known before
/// its result arrives.
pub(crate) enum ApprovalUpdate {
    /// A call waits for approval: the worker posts its card.
    Requested {
        /// The request's ID in the run, which is also the card's.
        id: ApprovalId,
        card: ApprovalCard,
    },
    Resolved {
        /// The request that ended; `None` when a session grant covered the call and nobody
        /// was asked.
        id: Option<ApprovalId>,
        tool_call_id: String,
        tool: String,
        result: ApprovalResult,
    },
}

/// Passes the approval events of a run on to the worker, which posts and closes the cards.
pub(crate) struct ApprovalEvents {
    pub(crate) updates: mpsc::Sender<ApprovalUpdate>,
    pub(crate) key: SessionKey,
    pub(crate) sink: Arc<dyn ServerSink>,
}

impl ApprovalEvents {
    /// Pass `event` on if it is about an approval.
    pub(crate) fn forward(&self, event: &Event) {
        let update = match &event.event {
            EventKind::ApprovalRequested {
                approval_id,
                tool_name,
                args,
                reason,
                ..
            } => ApprovalUpdate::Requested {
                id: ApprovalId(approval_id.clone()),
                card: ApprovalCard {
                    tool: tool_name.clone(),
                    summary: summarize(args),
                    reason: reason.clone(),
                },
            },
            EventKind::ApprovalResolved {
                approval_id,
                tool_call_id,
                tool_name,
                approval,
                ..
            } => ApprovalUpdate::Resolved {
                id: approval_id.clone().map(ApprovalId),
                tool_call_id: tool_call_id.clone(),
                tool: tool_name.clone(),
                result: *approval,
            },
            _ => return,
        };
        // Reading the run must not wait for the worker, which may be waiting for the run.
        // The channel holds the updates of approvals asked one at a time, two per approval,
        // and the worker drains it between its other steps, so it is not expected to fill.
        // If it did, a request would get no card and end when nobody answers it in time, or
        // a card would stay open. A closed channel means the worker has ended: nobody needs
        // the update.
        match self.updates.try_send(update) {
            Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
            Err(mpsc::error::TrySendError::Full(_)) => self.sink.emit(&ServerEvent::Failed {
                key: Some(self.key.clone()),
                error: "an approval update was dropped: its card is missing or stays open".into(),
            }),
        }
    }
}

/// The command of a bash call, or the arguments of any other call, at most
/// [`MAX_SUMMARY_CHARS`] characters long.
fn summarize(arguments: &Value) -> String {
    // `command` is the argument of the bash tool (`zx-builtin`), which this crate cannot see:
    // the one coupling to a tool here, so that a shell command reads as one on the card.
    match arguments.get("command").and_then(Value::as_str) {
        Some(command) => clip(command, MAX_SUMMARY_CHARS),
        None => clip(&arguments.to_string(), MAX_SUMMARY_CHARS),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_card_shows_the_command_of_a_shell_call() {
        assert_eq!(summarize(&json!({"command": "kubectl get pods"})), "kubectl get pods");
    }

    #[test]
    fn a_card_shows_the_arguments_of_other_calls() {
        assert_eq!(summarize(&json!({"path": "a.txt"})), r#"{"path":"a.txt"}"#);
    }

    #[test]
    fn a_long_command_is_cut_at_a_character_boundary() {
        let command = "界".repeat(MAX_SUMMARY_CHARS + 5);
        let summary = summarize(&json!({ "command": command }));
        assert_eq!(summary.chars().count(), MAX_SUMMARY_CHARS + "...".len());
        assert!(summary.ends_with("界..."));
    }

    #[test]
    fn a_command_of_exactly_the_limit_is_kept_whole() {
        let command = "a".repeat(MAX_SUMMARY_CHARS);
        assert_eq!(summarize(&json!({ "command": command.clone() })), command);
    }
}
