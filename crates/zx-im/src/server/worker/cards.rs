//! The conversation's approval cards: a card posted for each request, the request resolved
//! by an approver's click, and the card closed when the run reports how the approval ended.

use zx_agent::ai::now_ms;
use zx_agent::policy::ApprovalDecision;
use zx_agent::session::SessionError;

use super::{Phase, Worker};
use crate::server::approval::ApprovalUpdate;
use crate::server::{ApprovalRecord, ServerEvent};
use crate::{ApprovalCard, ApprovalId, Choice, MessageRef, Post, Replacement, UserId};

/// A card waiting for its result.
pub(super) struct Card {
    id: ApprovalId,
    /// What the card shows, shown again once it is closed.
    shown: ApprovalCard,
    /// The card as posted.
    posted: MessageRef,
    state: CardState,
}

/// How far a card's decision has come.
enum CardState {
    /// Nobody has clicked yet; the first approver's click decides.
    Open,
    /// Who decided.
    DecidedBy(UserId),
    /// A click came after the run stopped waiting; the card closes without a decider.
    Lapsed,
}

impl Worker {
    pub(super) async fn decide(&mut self, user: UserId, approval: ApprovalId, choice: Choice) {
        let Some(index) = self.cards.iter().position(|card| card.id == approval) else {
            // The card was closed already; a late click changes nothing.
            return;
        };
        if !self.out.shared.is_approver(&self.out.key.platform, &user) {
            self.out.post(self.out.replies().not_approver.clone()).await;
            return;
        }
        // The first approver's click decides; later clicks on the card change nothing.
        if !matches!(self.cards[index].state, CardState::Open) {
            return;
        }
        let decision = match choice {
            Choice::AllowOnce => ApprovalDecision::AllowOnce,
            Choice::Deny => ApprovalDecision::Deny(format!("denied in the chat by {user}")),
        };
        // The run may have stopped waiting (timeout or cancellation); its approval_resolved
        // event then closes the card without a decider.
        self.cards[index].state = match self.resolve(&approval, decision) {
            Ok(()) => CardState::DecidedBy(user),
            Err(_) => CardState::Lapsed,
        };
    }

    /// Resolve the approval request `id` of the run in progress with `decision`.
    fn resolve(&self, id: &ApprovalId, decision: ApprovalDecision) -> Result<(), SessionError> {
        match &self.phase {
            Phase::Running(running) => running.open.session.resolve_approval(&id.0, decision),
            // Without a run, no request waits.
            Phase::Idle { .. } | Phase::Waiting(_) => Err(SessionError::UnknownApproval),
        }
    }

    /// A call waits for approval: post its card. Without a card nobody can decide, so the
    /// call is denied, and the model hears why.
    async fn post_card(&mut self, id: ApprovalId, shown: ApprovalCard) {
        let request = Post::ApprovalRequest {
            id: id.clone(),
            card: shown.clone(),
        };
        match self.out.post_card(&request).await {
            Ok(posted) => self.cards.push(Card {
                id,
                shown,
                posted,
                state: CardState::Open,
            }),
            Err(error) => {
                // The request may have ended meanwhile; then there is nothing to deny.
                drop(self.resolve(&id, ApprovalDecision::Deny(error)));
            }
        }
    }

    pub(super) async fn approval(&mut self, update: ApprovalUpdate) {
        match update {
            ApprovalUpdate::Requested { id, card } => self.post_card(id, card).await,
            ApprovalUpdate::Resolved {
                id,
                tool_call_id,
                tool,
                result,
            } => {
                let card = self
                    .cards
                    .iter()
                    .position(|card| Some(&card.id) == id.as_ref())
                    .map(|index| self.cards.swap_remove(index));
                let (approval, user) = match card {
                    Some(card) => {
                        let user = match card.state {
                            CardState::DecidedBy(user) => Some(user),
                            CardState::Open | CardState::Lapsed => None,
                        };
                        let closed = Replacement::ApprovalResolved {
                            id: card.id.clone(),
                            card: card.shown,
                            user: user.clone(),
                            result,
                        };
                        // A card left open was reported; the decision stands either way.
                        self.out.replace(&card.posted, &closed).await;
                        (Some(card.id), user)
                    }
                    // No card: nobody was asked, or posting it failed.
                    None => (None, None),
                };
                self.out.shared.sink.emit(&ServerEvent::Approval(ApprovalRecord {
                    key: self.out.key.clone(),
                    tool_call_id,
                    tool,
                    approval,
                    user,
                    result,
                    ended_ms: now_ms(),
                }));
            }
        }
    }
}
