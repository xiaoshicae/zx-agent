//! The chain that asks the registered policies about a tool call: each bounded by its
//! timeout and the run's cancellation, and one that cannot answer denies the call.

use std::sync::Arc;
use std::time::Duration;

use super::{PolicyContext, PolicyDecision, ToolPolicy};
use crate::ai::ToolCall;
use crate::guard::{Priority, bounded};

/// The bound of one policy check unless the host gives another.
pub const DEFAULT_POLICY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct Registered {
    name: String,
    policy: Arc<dyn ToolPolicy>,
    timeout: Duration,
}

/// The policies of an agent, asked in the order they were registered. The strictest decision
/// stands, so the order never loosens one: a deny ends the check, and an ask outweighs an
/// allow. A clone is a chain of the same policies: each session of the agent asks one.
#[derive(Clone, Default)]
pub struct PolicyChain {
    policies: Vec<Registered>,
}

impl PolicyChain {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add `policy`, called `name` in the reason of a denial when it cannot answer. Each of
    /// its checks is bounded by [`DEFAULT_POLICY_TIMEOUT`].
    pub fn push(&mut self, name: impl Into<String>, policy: Arc<dyn ToolPolicy>) {
        self.push_with_timeout(name, policy, DEFAULT_POLICY_TIMEOUT);
    }

    /// Add `policy` as [`push`](Self::push) does, with `timeout` bounding each check.
    pub fn push_with_timeout(&mut self, name: impl Into<String>, policy: Arc<dyn ToolPolicy>, timeout: Duration) {
        self.policies.push(Registered {
            name: name.into(),
            policy,
            timeout,
        });
    }

    /// Add the policies of `other` after this chain's, each with the name and the timeout it
    /// was registered with.
    pub(crate) fn extend(&mut self, other: PolicyChain) {
        self.policies.extend(other.policies);
    }

    /// Whether no policy is registered: every call is then allowed.
    pub fn is_empty(&self) -> bool {
        self.policies.is_empty()
    }

    /// The decision on `call`: the first deny, else the first ask, else allow. A policy that
    /// panics, runs past its timeout or is cut short by cancellation denies the call, and the
    /// reason names it.
    pub(crate) async fn check(&self, call: &ToolCall, ctx: &PolicyContext) -> PolicyDecision {
        let mut strictest = PolicyDecision::Allow;
        for entry in &self.policies {
            let checked = bounded(
                entry.policy.check(call, ctx),
                entry.timeout,
                ctx.cancel.token(),
                Priority::Cancellation,
            )
            .await;
            let decision = match checked {
                Ok(decision) => decision,
                Err(stopped) => PolicyDecision::Deny(format!("policy {} {stopped}", entry.name)),
            };
            match decision {
                PolicyDecision::Deny(_) => return decision,
                PolicyDecision::Ask(_) if matches!(strictest, PolicyDecision::Allow) => strictest = decision,
                PolicyDecision::Ask(_) | PolicyDecision::Allow => {}
            }
        }
        strictest
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use super::*;

    fn context() -> PolicyContext {
        PolicyContext::new("session".into(), PathBuf::from("."), CancellationToken::new())
    }

    fn call() -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: "echo".into(),
            arguments: json!({}),
            arguments_error: None,
        }
    }

    struct Decides {
        decision: PolicyDecision,
        index: usize,
        seen: Arc<Mutex<Vec<usize>>>,
    }

    #[async_trait]
    impl ToolPolicy for Decides {
        async fn check(&self, _call: &ToolCall, _ctx: &PolicyContext) -> PolicyDecision {
            self.seen.lock().unwrap().push(self.index);
            self.decision.clone()
        }
    }

    #[tokio::test]
    async fn the_strictest_decision_stands_in_registration_order() {
        for (decisions, expected, visited) in [
            (
                vec![
                    PolicyDecision::Allow,
                    PolicyDecision::Ask("first".into()),
                    PolicyDecision::Ask("second".into()),
                ],
                PolicyDecision::Ask("first".into()),
                vec![0, 1, 2],
            ),
            (
                vec![
                    PolicyDecision::Ask("ask".into()),
                    PolicyDecision::Allow,
                    PolicyDecision::Deny("deny".into()),
                ],
                PolicyDecision::Deny("deny".into()),
                vec![0, 1, 2],
            ),
            (
                vec![PolicyDecision::Deny("first".into()), PolicyDecision::Allow],
                PolicyDecision::Deny("first".into()),
                vec![0],
            ),
        ] {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let mut chain = PolicyChain::new();
            for (index, decision) in decisions.into_iter().enumerate() {
                chain.push(
                    format!("policy{index}"),
                    Arc::new(Decides {
                        decision,
                        index,
                        seen: seen.clone(),
                    }),
                );
            }
            assert_eq!(chain.check(&call(), &context()).await, expected);
            assert_eq!(*seen.lock().unwrap(), visited);
        }
    }

    #[tokio::test]
    async fn no_policy_allows_every_call() {
        let chain = PolicyChain::new();
        assert!(chain.is_empty());
        assert_eq!(chain.check(&call(), &context()).await, PolicyDecision::Allow);
    }

    struct Never;

    #[async_trait]
    impl ToolPolicy for Never {
        async fn check(&self, _call: &ToolCall, _ctx: &PolicyContext) -> PolicyDecision {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn a_policy_past_its_timeout_denies_the_call_and_the_reason_names_it() {
        let mut chain = PolicyChain::new();
        chain.push_with_timeout("slow", Arc::new(Never), Duration::from_millis(1));
        assert_eq!(
            chain.check(&call(), &context()).await,
            PolicyDecision::Deny("policy slow timed out after 1ms".into())
        );
    }

    struct Panics;

    #[async_trait]
    impl ToolPolicy for Panics {
        async fn check(&self, _call: &ToolCall, _ctx: &PolicyContext) -> PolicyDecision {
            panic!("policy bug");
        }
    }

    #[tokio::test]
    async fn a_panicking_policy_denies_the_call_whatever_the_others_allow() {
        let mut chain = PolicyChain::new();
        chain.push("broken", Arc::new(Panics));
        assert_eq!(
            chain.check(&call(), &context()).await,
            PolicyDecision::Deny("policy broken panicked".into())
        );
    }

    #[tokio::test]
    async fn a_cancelled_run_denies_the_call_a_policy_is_still_checking() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let ctx = PolicyContext::new("session".into(), PathBuf::from("."), cancel);
        let mut chain = PolicyChain::new();
        chain.push("waiting", Arc::new(Never));
        assert_eq!(
            chain.check(&call(), &ctx).await,
            PolicyDecision::Deny("policy waiting was cancelled".into())
        );
    }
}
