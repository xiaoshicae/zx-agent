//! Approval requests travel to the single input owner; callbacks never read stdin.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};
use zx_agent::interaction::{Approval, ApprovalRequest, Interaction};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RequestId(pub(super) u64);

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

pub(super) struct Ticket {
    pub(super) id: RequestId,
    pub(super) request: ApprovalRequest,
    pub(super) reply: oneshot::Sender<Approval>,
}

pub(super) struct TerminalInteraction {
    sender: mpsc::Sender<Ticket>,
    next_id: AtomicU64,
}

impl TerminalInteraction {
    pub(super) fn channel() -> (Self, mpsc::Receiver<Ticket>) {
        let (sender, receiver) = mpsc::channel(1);
        (
            Self {
                sender,
                next_id: AtomicU64::new(1),
            },
            receiver,
        )
    }

    fn allocate_id(&self) -> Result<RequestId, String> {
        let mut current = self.next_id.load(Ordering::SeqCst);
        loop {
            let next = current.checked_add(1).ok_or("terminal approval IDs exhausted")?;
            match self
                .next_id
                .compare_exchange_weak(current, next, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return Ok(RequestId(current)),
                Err(observed) => current = observed,
            }
        }
    }
}

#[async_trait]
impl Interaction for TerminalInteraction {
    async fn confirm(&self, request: &ApprovalRequest) -> Result<Approval, String> {
        let id = self.allocate_id()?;
        let (reply, response) = oneshot::channel();
        self.sender
            .send(Ticket {
                id,
                request: request.clone(),
                reply,
            })
            .await
            .map_err(|_| "terminal input is unavailable".to_string())?;
        response
            .await
            .map_err(|_| "terminal approval was cancelled or closed".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use zx_agent::ai::ToolCall;

    fn request() -> ApprovalRequest {
        ApprovalRequest {
            call: ToolCall {
                id: "c".into(),
                name: "bash".into(),
                arguments: json!({"command":"echo ok"}),
                arguments_error: None,
            },
            reason: "review".into(),
        }
    }

    #[tokio::test]
    async fn confirmation_waits_for_the_single_input_owner() {
        let (interaction, mut input) = TerminalInteraction::channel();
        let request = request();
        let mut confirmation = Box::pin(interaction.confirm(&request));
        let ticket = tokio::select! {
            ticket = input.recv() => ticket.unwrap(),
            _ = &mut confirmation => panic!("confirmation must wait"),
        };
        assert_eq!(ticket.request, request);
        assert_eq!(ticket.id, RequestId(1));
        ticket.reply.send(Approval::AllowOnce).unwrap();
        assert_eq!(confirmation.await.unwrap(), Approval::AllowOnce);
    }

    #[tokio::test]
    async fn dropping_confirmation_invalidates_its_reply_and_the_next_id_is_distinct() {
        let (interaction, mut input) = TerminalInteraction::channel();
        let request = request();
        let mut confirmation = Box::pin(interaction.confirm(&request));
        let old = tokio::select! {
            ticket = input.recv() => ticket.unwrap(),
            _ = &mut confirmation => panic!("confirmation must wait"),
        };
        drop(confirmation);
        assert!(old.reply.is_closed());
        let mut next = Box::pin(interaction.confirm(&request));
        let ticket = tokio::select! {
            ticket = input.recv() => ticket.unwrap(),
            _ = &mut next => panic!("confirmation must wait"),
        };
        assert_ne!(ticket.id, old.id);
        ticket.reply.send(Approval::Deny("no".into())).unwrap();
        assert_eq!(next.await.unwrap(), Approval::Deny("no".into()));
    }

    #[tokio::test]
    async fn a_missing_input_owner_is_an_error() {
        let (interaction, input) = TerminalInteraction::channel();
        drop(input);
        assert!(
            interaction
                .confirm(&request())
                .await
                .unwrap_err()
                .contains("unavailable")
        );
    }

    #[tokio::test]
    async fn exhausted_ids_fail_closed_without_reusing_an_approval_number() {
        let (interaction, mut input) = TerminalInteraction::channel();
        interaction.next_id.store(u64::MAX, Ordering::SeqCst);
        for _ in 0..2 {
            assert!(interaction.confirm(&request()).await.unwrap_err().contains("exhausted"));
        }
        assert!(input.try_recv().is_err());
    }
}
