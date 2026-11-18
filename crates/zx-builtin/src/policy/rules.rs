//! Rules by tool name, with the auto-approval list that lets a rule's Ask become Allow for the
//! calls it covers.

use async_trait::async_trait;

use super::auto_approve::AutoApprove;
use crate::ai::ToolCall;
use zx_core::policy::{PolicyContext, PolicyDecision, ToolPolicy};

/// The calls a rule matches. Bash commands are told apart by [`AutoApprove`], not by rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuleMatcher {
    Any,
    Tool(String),
}

/// One rule: the calls it matches and the verdict on them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    /// The calls the rule applies to.
    pub matcher: RuleMatcher,
    /// The verdict on a matching call.
    pub decision: PolicyDecision,
}

impl Rule {
    fn matches(&self, tool: &str) -> bool {
        match &self.matcher {
            RuleMatcher::Any => true,
            RuleMatcher::Tool(name) => name == tool,
        }
    }
}

/// Deny rules are tried before Ask, then Allow; registration order breaks ties.
/// A call with no matching rule is denied. With an [`AutoApprove`] list, an Ask verdict for
/// a call the list covers becomes Allow; Deny verdicts are never changed.
pub struct RulePolicy {
    rules: Vec<Rule>,
    auto_approve: AutoApprove,
}

impl RulePolicy {
    /// A policy of `rules` alone, with nothing auto-approved.
    pub fn new(rules: Vec<Rule>) -> Self {
        Self {
            rules,
            auto_approve: AutoApprove::default(),
        }
    }

    /// Let the calls that `list` covers run where a rule of this policy would ask.
    pub fn auto_approve(mut self, list: AutoApprove) -> Self {
        self.auto_approve = list;
        self
    }

    /// The first matching rule of the highest-ranking kind: Deny before Ask before Allow.
    fn rule_verdict(&self, tool_name: &str) -> PolicyDecision {
        let rank = |decision: &PolicyDecision| match decision {
            PolicyDecision::Deny(_) => 0,
            PolicyDecision::Ask(_) => 1,
            PolicyDecision::Allow => 2,
        };
        self.rules
            .iter()
            .filter(|rule| rule.matches(tool_name))
            .min_by_key(|rule| rank(&rule.decision))
            .map_or_else(
                || PolicyDecision::Deny(format!("no policy rule permits {tool_name:?}")),
                |rule| rule.decision.clone(),
            )
    }

    /// The verdict on `call`: the strictest rule that matches its tool, with an ask on a call
    /// the auto-approval list covers turned into an allow.
    pub fn verdict(&self, call: &ToolCall) -> PolicyDecision {
        let verdict = self.rule_verdict(&call.name);
        if matches!(verdict, PolicyDecision::Ask(_)) && self.auto_approve.covers_call(call) {
            return PolicyDecision::Allow;
        }
        verdict
    }
}

#[async_trait]
impl ToolPolicy for RulePolicy {
    async fn check(&self, call: &ToolCall, _ctx: &PolicyContext) -> PolicyDecision {
        self.verdict(call)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::policy::test_support::call;

    fn denied(decision: &PolicyDecision) -> bool {
        matches!(decision, PolicyDecision::Deny(_))
    }

    #[test]
    fn rule_priority_is_deny_then_ask_then_allow_regardless_of_registration() {
        let policy = RulePolicy::new(vec![
            Rule {
                matcher: RuleMatcher::Any,
                decision: PolicyDecision::Allow,
            },
            Rule {
                matcher: RuleMatcher::Tool("bash".into()),
                decision: PolicyDecision::Ask("first ask".into()),
            },
            Rule {
                matcher: RuleMatcher::Any,
                decision: PolicyDecision::Ask("second ask".into()),
            },
            Rule {
                matcher: RuleMatcher::Tool("bash".into()),
                decision: PolicyDecision::Deny("first deny".into()),
            },
            Rule {
                matcher: RuleMatcher::Tool("bash".into()),
                decision: PolicyDecision::Deny("second deny".into()),
            },
        ]);
        assert_eq!(
            policy.verdict(&call("bash", json!({"command": "rm x"}))),
            PolicyDecision::Deny("first deny".into())
        );
        assert_eq!(
            policy.verdict(&call("read_file", json!({}))),
            PolicyDecision::Ask("second ask".into())
        );
        let policy = RulePolicy::new(vec![
            Rule {
                matcher: RuleMatcher::Any,
                decision: PolicyDecision::Allow,
            },
            Rule {
                matcher: RuleMatcher::Tool("bash".into()),
                decision: PolicyDecision::Ask("first".into()),
            },
            Rule {
                matcher: RuleMatcher::Any,
                decision: PolicyDecision::Ask("second".into()),
            },
        ]);
        assert_eq!(
            policy.verdict(&call("bash", json!({}))),
            PolicyDecision::Ask("first".into())
        );
    }

    #[test]
    fn unmatched_calls_fail_closed() {
        let policy = RulePolicy::new(vec![Rule {
            matcher: RuleMatcher::Tool("read_file".into()),
            decision: PolicyDecision::Allow,
        }]);
        assert!(denied(&policy.verdict(&call("bash", json!({"command": "ls"})))));
        assert_eq!(policy.verdict(&call("read_file", json!({}))), PolicyDecision::Allow);
    }

    #[test]
    fn auto_approval_turns_only_a_bash_ask_into_allow() {
        let policy = RulePolicy::new(vec![
            Rule {
                matcher: RuleMatcher::Tool("bash".into()),
                decision: PolicyDecision::Ask("approve bash".into()),
            },
            Rule {
                matcher: RuleMatcher::Any,
                decision: PolicyDecision::Ask("approve any".into()),
            },
        ])
        .auto_approve(AutoApprove::new(["ls"]));
        assert_eq!(
            policy.verdict(&call("bash", json!({"command": "ls -a"}))),
            PolicyDecision::Allow
        );
        assert_eq!(
            policy.verdict(&call("bash", json!({"command": "rm x"}))),
            PolicyDecision::Ask("approve bash".into())
        );
        assert_eq!(
            policy.verdict(&call("bash", json!({}))),
            PolicyDecision::Ask("approve bash".into())
        );
        assert_eq!(
            policy.verdict(&call("read_file", json!({"command": "ls"}))),
            PolicyDecision::Ask("approve any".into())
        );
    }

    #[test]
    fn auto_approval_never_overrides_a_denial() {
        let list = AutoApprove::new(["ls"]);
        let ls = call("bash", json!({"command": "ls -a"}));
        let denied_by_rule = RulePolicy::new(vec![Rule {
            matcher: RuleMatcher::Tool("bash".into()),
            decision: PolicyDecision::Deny("denied".into()),
        }])
        .auto_approve(list.clone());
        assert_eq!(denied_by_rule.verdict(&ls), PolicyDecision::Deny("denied".into()));
        let unmatched = RulePolicy::new(Vec::new()).auto_approve(list);
        assert!(denied(&unmatched.verdict(&ls)));
    }
}
