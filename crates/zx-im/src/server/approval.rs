//! Approvals in the chat: a card with buttons, decided by an approver's click.
//!
//! The task that reads a run passes its approval events on to the worker
//! ([`RunEvents`](super::progress::RunEvents)): the worker posts a card for each request,
//! resolves the request when an approver clicks, and closes the card when the run reports how
//! the approval ended.

use serde_json::Value;
use zx_agent::events::{ApprovalResult, Event, EventKind};
use zx_util::text::clip;

use crate::{ApprovalCard, ApprovalId};

/// Longest command or argument text shown on a card; longer text ends with "...". A card is
/// for deciding at a glance; the full call is in the session file.
const MAX_SUMMARY_CHARS: usize = 1500;

/// What the worker learns about approvals, in order: a call waits for one, then the run
/// reported how it ended. Both travel through one channel, so the request is known before
/// its result arrives.
pub(super) enum ApprovalUpdate {
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
        /// When the run reported it, in milliseconds since the Unix epoch: when the approval
        /// ended, which is before its card closes.
        at_ms: u64,
    },
}

/// What `event` tells the worker about an approval; `None` when it is not about one.
pub(super) fn approval_update(event: &Event) -> Option<ApprovalUpdate> {
    match &event.event {
        EventKind::ApprovalRequested {
            approval_id,
            tool_name,
            args,
            reason,
            ..
        } => Some(ApprovalUpdate::Requested {
            id: ApprovalId(approval_id.clone()),
            card: ApprovalCard {
                tool: tool_name.clone(),
                summary: summarize(args),
                reason: reason.clone(),
            },
        }),
        EventKind::ApprovalResolved {
            approval_id,
            tool_call_id,
            tool_name,
            approval,
            ..
        } => Some(ApprovalUpdate::Resolved {
            id: approval_id.clone().map(ApprovalId),
            tool_call_id: tool_call_id.clone(),
            tool: tool_name.clone(),
            result: *approval,
            at_ms: event.at_ms,
        }),
        // Nothing about approvals: a new event about them is to be told apart above.
        EventKind::RunStart { .. }
        | EventKind::RunEnd { .. }
        | EventKind::AttemptStart { .. }
        | EventKind::StepStart { .. }
        | EventKind::RequestStart { .. }
        | EventKind::MessageStart { .. }
        | EventKind::MessageUpdate { .. }
        | EventKind::MessageEnd { .. }
        | EventKind::CustomEntry { .. }
        | EventKind::ToolExecutionStart { .. }
        | EventKind::ToolExecutionUpdate { .. }
        | EventKind::ToolDecision { .. }
        | EventKind::ToolExecutionEnd { .. }
        | EventKind::StepEnd { .. }
        | EventKind::AttemptEnd { .. }
        | EventKind::RetryScheduled { .. }
        | EventKind::ContextOverflow { .. }
        | EventKind::ContextEdited { .. }
        | EventKind::HookFailed { .. }
        | EventKind::CompactionStart { .. }
        | EventKind::CompactionEnd { .. }
        | EventKind::CompactionFailed { .. }
        | EventKind::ToolCallRecovered { .. } => None,
    }
}

/// The command of a bash call, or the arguments of any other call, at most
/// [`MAX_SUMMARY_CHARS`] characters long.
fn summarize(arguments: &Value) -> String {
    // `command` is the argument of the bash tool (`zx_agent::builtin`), which names it in no
    // constant: the one coupling to a tool here, so that a shell command reads as one on the
    // card.
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
