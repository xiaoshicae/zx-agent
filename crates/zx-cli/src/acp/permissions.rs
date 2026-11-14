//! Approvals as `session/request_permission`: the client answers with one of the offered
//! options, or `cancelled` once it cancelled the turn.

use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest, SessionId, ToolCallId,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::{Client, Error};
use serde_json::Value;

use zx_agent::Session;
use zx_agent::policy::ApprovalDecision;

use super::tool_kind;
use crate::render::call_label;

/// The option IDs offered with every permission request, in the order they are shown.
const ALLOW_ONCE: &str = "allow-once";
const ALLOW_SESSION: &str = "allow-session";
const REJECT_ONCE: &str = "reject-once";

/// A call that waits for approval, as the run announced it.
pub(super) struct Request {
    /// The request's ID in the run, which its answer names.
    pub(super) approval_id: String,
    pub(super) tool_call_id: String,
    pub(super) tool: String,
    /// The arguments the call would run with.
    pub(super) arguments: Value,
}

/// Ask the client to approve `request` of session `session_id`. The client's answer goes to
/// `session` whenever it comes, from a task of its own: the turn keeps being read meanwhile,
/// so its timeout and its cancellation take effect at once. An answer for a request the run
/// no longer waits on changes nothing.
pub(super) fn ask(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    session: &Session,
    request: Request,
) -> Result<(), Error> {
    let fields = ToolCallUpdateFields::new()
        .title(call_label(&request.tool, &request.arguments))
        .kind(tool_kind(&request.tool))
        .status(ToolCallStatus::Pending)
        .raw_input(request.arguments);
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
        session_id.clone(),
        ToolCallUpdate::new(ToolCallId::new(request.tool_call_id.as_str()), fields),
        options,
    );
    let (asking, session, approval_id) = (cx.clone(), session.clone(), request.approval_id);
    cx.spawn(async move {
        let decision = match asking.send_request(permission).block_task().await {
            Ok(response) => decision(&response.outcome),
            Err(error) => ApprovalDecision::Deny(format!("the permission request failed: {error}")),
        };
        // The run may have stopped waiting: it timed out, or the turn was cancelled.
        drop(session.resolve_approval(&approval_id, decision));
        Ok(())
    })
}

/// What the client's answer means for the call.
fn decision(outcome: &RequestPermissionOutcome) -> ApprovalDecision {
    match outcome {
        RequestPermissionOutcome::Cancelled => ApprovalDecision::Deny("the client cancelled the turn".into()),
        RequestPermissionOutcome::Selected(selected) => match &*selected.option_id.0 {
            ALLOW_ONCE => ApprovalDecision::AllowOnce,
            ALLOW_SESSION => ApprovalDecision::AllowSession,
            _ => ApprovalDecision::Deny("rejected by the user".into()),
        },
        // The protocol may add outcomes: the enum is non-exhaustive, and an unknown one
        // never authorizes a call.
        _ => ApprovalDecision::Deny("the client answered with an unknown outcome".into()),
    }
}
