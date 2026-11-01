//! Approvals in the chat: a card with buttons, answered by an approver's click.
//!
//! [`ImApproval::confirm`] posts the card and waits for the worker to pass on a decision.
//! The card is closed by the worker when the run reports `approval_resolved`, because the
//! runtime drops `confirm` on timeout or cancellation (docs/serve-im-review.md, section 3.3).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::agent::{AgentEvent, ApprovalResult, Event, EventSink};
use crate::ai::ToolCall;
use crate::interaction::{Approval, ApprovalRequest, Interaction};

use super::{
    ApprovalId, Choice, Destination, ImChannel, MessageRef, Outbound, ServerEvent, ServerSink, SessionKey, UserId,
};

/// Longest command or argument text shown on a card; longer text ends with "…". A card is
/// for deciding at a glance; the full call is in the session file.
const MAX_SUMMARY_CHARS: usize = 1500;

/// What the worker learns about approvals, in order: a card was posted, then the run
/// reported how the approval ended. Both travel through one channel, so the card is known
/// before its result arrives.
pub(crate) enum ApprovalUpdate {
    Posted(Posted),
    Resolved {
        tool_call_id: String,
        tool: String,
        result: ApprovalResult,
    },
}

/// A posted card that waits for a decision.
pub(crate) struct Posted {
    pub(crate) id: ApprovalId,
    pub(crate) tool_call_id: String,
    pub(crate) card: MessageRef,
    pub(crate) decide: oneshot::Sender<Decided>,
}

/// An approver's click.
pub(crate) struct Decided {
    pub(crate) user: UserId,
    pub(crate) choice: Choice,
}

/// The interaction host of one conversation: posts approval cards to its thread.
pub(crate) struct ImApproval {
    pub(crate) channel: Arc<dyn ImChannel>,
    pub(crate) to: Destination,
    pub(crate) updates: mpsc::Sender<ApprovalUpdate>,
    pub(crate) outbound_timeout: Duration,
}

#[async_trait]
impl Interaction for ImApproval {
    async fn confirm(&self, request: &ApprovalRequest) -> Result<Approval, String> {
        let id = ApprovalId(Uuid::new_v4().to_string());
        let card = Outbound::ApprovalRequest {
            id: id.clone(),
            tool: request.call.name.clone(),
            summary: summarize(&request.call),
            reason: request.reason.clone(),
        };
        let card = match tokio::time::timeout(self.outbound_timeout, self.channel.send(&self.to, &card)).await {
            Ok(Ok(posted)) => posted,
            Ok(Err(error)) => return Err(format!("cannot post the approval request: {error}")),
            Err(_) => {
                return Err(format!(
                    "posting the approval request took longer than {} s",
                    self.outbound_timeout.as_secs()
                ));
            }
        };
        let (decide, decided) = oneshot::channel();
        self.updates
            .send(ApprovalUpdate::Posted(Posted {
                id,
                tool_call_id: request.call.id.clone(),
                card,
                decide,
            }))
            .await
            .map_err(|_| "the conversation closed before the approval request was registered".to_string())?;
        let decided = decided
            .await
            .map_err(|_| "the conversation closed before anyone decided".to_string())?;
        Ok(match decided.choice {
            Choice::AllowOnce => Approval::AllowOnce,
            Choice::Deny => Approval::Deny(format!("denied in the chat by {}", decided.user)),
        })
    }
}

/// Passes `approval_resolved` events on to the worker, which closes the card.
pub(crate) struct ApprovalEvents {
    pub(crate) updates: mpsc::Sender<ApprovalUpdate>,
    pub(crate) key: SessionKey,
    pub(crate) sink: Arc<dyn ServerSink>,
}

impl EventSink for ApprovalEvents {
    fn emit(&self, event: &Event) {
        let AgentEvent::ApprovalResolved {
            tool_call_id,
            tool_name,
            approval,
            ..
        } = &event.event
        else {
            return;
        };
        let update = ApprovalUpdate::Resolved {
            tool_call_id: tool_call_id.clone(),
            tool: tool_name.clone(),
            result: *approval,
        };
        // Callbacks must not wait. The channel holds the updates of approvals asked one at a
        // time, two per approval, and the worker drains it between its other steps, so it is
        // not expected to fill; if it did, the card would stay open and the record would be
        // missing. A closed channel means the worker has ended: nobody needs the update.
        match self.updates.try_send(update) {
            Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
            Err(mpsc::error::TrySendError::Full(_)) => self.sink.emit(&ServerEvent::Failed {
                key: Some(self.key.clone()),
                error: format!("the result of approval {tool_call_id} was dropped; its card stays open"),
            }),
        }
    }
}

/// The command of a bash call, or the arguments of any other call, at most
/// [`MAX_SUMMARY_CHARS`] characters long.
pub(crate) fn summarize(call: &ToolCall) -> String {
    let text = match call.arguments.get("command").and_then(Value::as_str) {
        Some(command) => command.to_string(),
        None => call.arguments.to_string(),
    };
    match text.char_indices().nth(MAX_SUMMARY_CHARS) {
        // char_indices yields character boundaries, so the slice cannot split a character.
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn call(arguments: Value) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments,
            arguments_error: None,
        }
    }

    #[test]
    fn a_card_shows_the_command_of_a_shell_call() {
        assert_eq!(
            summarize(&call(json!({"command": "kubectl get pods"}))),
            "kubectl get pods"
        );
    }

    #[test]
    fn a_card_shows_the_arguments_of_other_calls() {
        assert_eq!(summarize(&call(json!({"path": "a.txt"}))), r#"{"path":"a.txt"}"#);
    }

    #[test]
    fn a_long_command_is_cut_at_a_character_boundary() {
        let command = "界".repeat(MAX_SUMMARY_CHARS + 5);
        let summary = summarize(&call(json!({ "command": command })));
        assert_eq!(summary.chars().count(), MAX_SUMMARY_CHARS + 1);
        assert!(summary.ends_with("界…"));
    }

    #[test]
    fn a_command_of_exactly_the_limit_is_kept_whole() {
        let command = "a".repeat(MAX_SUMMARY_CHARS);
        assert_eq!(summarize(&call(json!({ "command": command.clone() }))), command);
    }
}
