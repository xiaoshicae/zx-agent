//! Approval is supplied by the host; the runtime does not read from a terminal.

use async_trait::async_trait;
use serde_json::Value;
use std::time::Duration;

use crate::ai::ToolCall;

/// Default wait for one host confirmation; frontends can override it in LoopConfig.
pub const DEFAULT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);

/// The exact tool call and the policy's reason for asking.
#[derive(Clone, Debug, PartialEq)]
pub struct ApprovalRequest {
    pub call: ToolCall,
    pub reason: String,
}

/// Scope of an approval. Session grants cover only the exact tool name and arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Approval {
    AllowOnce,
    AllowSession,
    Deny(String),
}

/// A frontend or embedding host that can answer approval requests.
///
/// Confirmation must be asynchronous and must not block the executor. The runtime
/// bounds the wait and treats errors, panics, timeouts and cancellation as rejection.
#[async_trait]
pub trait Interaction: Send + Sync {
    async fn confirm(&self, request: &ApprovalRequest) -> Result<Approval, String>;

    /// Optional notification supplied by the host; must return promptly.
    fn notify(&self, _text: &str) {}
}

/// Unattended interaction: an Ask verdict always becomes a denial.
pub struct NoHuman;

#[async_trait]
impl Interaction for NoHuman {
    async fn confirm(&self, _request: &ApprovalRequest) -> Result<Approval, String> {
        Ok(Approval::Deny("no human is available to approve this call".to_string()))
    }
}

/// In-memory approvals for one conversation; never persisted or shared implicitly.
#[derive(Default)]
pub struct ApprovalCache {
    calls: Vec<(String, Value)>,
}

impl ApprovalCache {
    /// Revoke every session approval.
    pub fn clear(&mut self) {
        self.calls.clear();
    }

    pub(crate) fn allows(&self, call: &ToolCall) -> bool {
        self.calls
            .iter()
            .any(|(name, args)| name == &call.name && args == &call.arguments)
    }

    pub(crate) fn remember(&mut self, call: &ToolCall) {
        if !self.allows(call) {
            self.calls.push((call.name.clone(), call.arguments.clone()));
        }
    }
}
