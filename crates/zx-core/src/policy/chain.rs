//! The chain that asks the registered policies about a tool call: each bounded by its
//! timeout and the run's cancellation, with panics turned into failures.

use std::sync::Arc;
use std::time::Duration;

use super::{PolicyContext, PolicyDecision, ToolPolicy};
use crate::ai::ToolCall;
use crate::hooks::HookCall;
use crate::{DEFAULT_HOOK_TIMEOUT, HookError, HookPoint};

struct Registered {
    name: String,
    policy: Arc<dyn ToolPolicy>,
    timeout: Duration,
}

/// The policies of a loop, asked in the order they were registered. The strictest verdict
/// stands, so the order never loosens one: a deny ends the check, and an ask outweighs an
/// allow.
#[derive(Default)]
pub struct PolicyChain {
    policies: Vec<Registered>,
}

impl PolicyChain {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add `policy`, called `name` in `hook_failed` events and traces. Each of its checks is
    /// bounded by [`DEFAULT_HOOK_TIMEOUT`].
    pub fn push(&mut self, name: impl Into<String>, policy: Arc<dyn ToolPolicy>) {
        self.push_with_timeout(name, policy, DEFAULT_HOOK_TIMEOUT);
    }

    /// Add `policy` as [`push`](Self::push) does, with `timeout` bounding each check.
    pub fn push_with_timeout(&mut self, name: impl Into<String>, policy: Arc<dyn ToolPolicy>, timeout: Duration) {
        self.policies.push(Registered {
            name: name.into(),
            policy,
            timeout,
        });
    }

    /// Whether no policy is registered: every call is then allowed.
    pub fn is_empty(&self) -> bool {
        self.policies.is_empty()
    }

    /// The verdict on `call`: the first deny, else the first ask, else allow. A policy that
    /// panics, runs past its timeout or is cut short by cancellation is an error, which the
    /// caller treats as a denial.
    pub(crate) async fn check(&self, call: &ToolCall, ctx: &PolicyContext) -> Result<PolicyDecision, HookError> {
        let mut verdict = PolicyDecision::Allow;
        for entry in &self.policies {
            let guard = HookCall {
                name: &entry.name,
                phase: HookPoint::Policy,
                timeout: entry.timeout,
            };
            let decision = guard
                .run(async { Ok(entry.policy.check(call, ctx).await) }, ctx.cancel.token())
                .await?;
            match decision {
                PolicyDecision::Deny(_) => return Ok(decision),
                PolicyDecision::Ask(_) if matches!(verdict, PolicyDecision::Allow) => verdict = decision,
                PolicyDecision::Ask(_) | PolicyDecision::Allow => {}
            }
        }
        Ok(verdict)
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
    use crate::HookFailure;

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

    struct Verdict {
        decision: PolicyDecision,
        index: usize,
        seen: Arc<Mutex<Vec<usize>>>,
    }

    #[async_trait]
    impl ToolPolicy for Verdict {
        async fn check(&self, _call: &ToolCall, _ctx: &PolicyContext) -> PolicyDecision {
            self.seen.lock().unwrap().push(self.index);
            self.decision.clone()
        }
    }

    #[tokio::test]
    async fn the_strictest_verdict_stands_in_registration_order() {
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
                    Arc::new(Verdict {
                        decision,
                        index,
                        seen: seen.clone(),
                    }),
                );
            }
            assert_eq!(chain.check(&call(), &context()).await.unwrap(), expected);
            assert_eq!(*seen.lock().unwrap(), visited);
        }
    }

    #[tokio::test]
    async fn no_policy_allows_every_call() {
        let chain = PolicyChain::new();
        assert!(chain.is_empty());
        assert_eq!(chain.check(&call(), &context()).await.unwrap(), PolicyDecision::Allow);
    }

    struct Never;

    #[async_trait]
    impl ToolPolicy for Never {
        async fn check(&self, _call: &ToolCall, _ctx: &PolicyContext) -> PolicyDecision {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn a_policy_past_its_timeout_is_a_failure_that_names_it() {
        let mut chain = PolicyChain::new();
        chain.push_with_timeout("slow", Arc::new(Never), Duration::from_millis(1));
        let error = chain.check(&call(), &context()).await.unwrap_err();
        assert_eq!((error.hook.as_str(), error.phase), ("slow", HookPoint::Policy));
        assert!(matches!(error.failure, HookFailure::TimedOut(_)), "{error:?}");
    }
}
