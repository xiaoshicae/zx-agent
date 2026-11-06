//! Approvals as `session/request_permission`: the client answers with one of the offered
//! options, or `cancelled` once it cancelled the turn.

use agent_client_protocol::Client;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest, SessionId, ToolCallId,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use async_trait::async_trait;

use zx_agent::interaction::{Approval, ApprovalRequest, Interaction};

use super::tool_kind;
use crate::render::call_label;

/// The option IDs offered with every permission request, in the order they are shown.
const ALLOW_ONCE: &str = "allow-once";
const ALLOW_SESSION: &str = "allow-session";
const REJECT_ONCE: &str = "reject-once";

/// The client as the approver of one session.
pub(super) struct Permissions {
    pub(super) cx: ConnectionTo<Client>,
    pub(super) session_id: SessionId,
}

#[async_trait]
impl Interaction for Permissions {
    async fn confirm(&self, request: &ApprovalRequest) -> Result<Approval, String> {
        let call = &request.call;
        let fields = ToolCallUpdateFields::new()
            .title(call_label(&call.name, &call.arguments))
            .kind(tool_kind(&call.name))
            .status(ToolCallStatus::Pending)
            .raw_input(call.arguments.clone());
        let options = vec![
            PermissionOption::new(ALLOW_ONCE, "Allow once", PermissionOptionKind::AllowOnce),
            PermissionOption::new(
                ALLOW_SESSION,
                "Allow identical calls in this session",
                PermissionOptionKind::AllowAlways,
            ),
            PermissionOption::new(REJECT_ONCE, "Reject", PermissionOptionKind::RejectOnce),
        ];
        let permission = RequestPermissionRequest::new(
            self.session_id.clone(),
            ToolCallUpdate::new(ToolCallId::new(call.id.as_str()), fields),
            options,
        );
        // This runs inside the turn, a spawned task, so waiting here does not hold the
        // dispatch loop that delivers the answer.
        let response = self
            .cx
            .send_request(permission)
            .block_task()
            .await
            .map_err(|error| format!("the permission request failed: {error}"))?;
        Ok(match response.outcome {
            RequestPermissionOutcome::Cancelled => Approval::Deny("the client cancelled the turn".into()),
            RequestPermissionOutcome::Selected(selected) => match &*selected.option_id.0 {
                ALLOW_ONCE => Approval::AllowOnce,
                ALLOW_SESSION => Approval::AllowSession,
                _ => Approval::Deny("rejected by the user".into()),
            },
            // The protocol may add outcomes: the enum is non-exhaustive, and an unknown one
            // never authorizes a call.
            _ => Approval::Deny("the client answered with an unknown outcome".into()),
        })
    }
}
