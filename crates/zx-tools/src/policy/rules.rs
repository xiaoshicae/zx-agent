//! Rules by tool name or bash command prefix, with the auto-approval list that lets a rule's
//! Ask become Allow.

use async_trait::async_trait;
use serde_json::Value;

use super::auto_approve::AutoApprove;
use super::command::ALLOWLIST_FORBIDDEN;
use super::{begins_with, command_of, normalize};
use crate::agent::{Decision, ToolPolicy};
use crate::tools::BASH;

/// A lexical match, not a shell parser or an execution sandbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuleMatcher {
    Any,
    Tool(String),
    /// Matches bash's command at a word boundary, compared like the allowlist of
    /// [`CommandPolicy`](super::CommandPolicy) (lowercased, whitespace collapsed). An Allow
    /// rule does not match a command with a pipe or with syntax the allowlist rejects, so it
    /// cannot let a second command through. Quoting is not interpreted: a Deny prefix stops
    /// the plain spelling only. Use CommandPolicy as well when shell syntax must be restricted.
    CommandPrefix(String),
}

/// One rule: the calls it matches and the verdict on them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    /// The calls the rule applies to.
    pub matcher: RuleMatcher,
    /// The verdict on a matching call.
    pub decision: Decision,
}

impl Rule {
    fn matches(&self, tool: &str, args: &Value) -> bool {
        match &self.matcher {
            RuleMatcher::Any => true,
            RuleMatcher::Tool(name) => name == tool,
            RuleMatcher::CommandPrefix(prefix) => {
                let prefix = normalize(prefix);
                if tool != BASH || prefix.is_empty() {
                    return false;
                }
                command_of(args).is_some_and(|command| {
                    let chains =
                        command.contains('|') || ALLOWLIST_FORBIDDEN.iter().any(|token| command.contains(token));
                    !(chains && self.decision == Decision::Allow) && begins_with(&normalize(command), &prefix)
                })
            }
        }
    }
}

/// Deny rules are tried before Ask, then Allow; registration order breaks ties.
/// A call with no matching rule is denied. With an [`AutoApprove`] list, an Ask verdict for
/// a bash command the list covers becomes Allow; Deny verdicts are never changed.
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

    /// Let bash commands covered by `list` run where a rule of this policy would ask.
    pub fn auto_approve(mut self, list: AutoApprove) -> Self {
        self.auto_approve = list;
        self
    }

    /// The first matching rule of the highest-ranking kind: Deny before Ask before Allow.
    fn rule_verdict(&self, tool_name: &str, args: &Value) -> Decision {
        let rank = |decision: &Decision| match decision {
            Decision::Deny(_) => 0,
            Decision::Ask(_) => 1,
            Decision::Allow => 2,
        };
        self.rules
            .iter()
            .filter(|rule| rule.matches(tool_name, args))
            .min_by_key(|rule| rank(&rule.decision))
            .map_or_else(
                || Decision::Deny(format!("no policy rule permits {tool_name:?}")),
                |rule| rule.decision.clone(),
            )
    }
}

#[async_trait]
impl ToolPolicy for RulePolicy {
    async fn check(&self, tool_name: &str, args: &Value) -> Decision {
        let verdict = self.rule_verdict(tool_name, args);
        let covered = tool_name == BASH && command_of(args).is_some_and(|command| self.auto_approve.covers(command));
        if covered && matches!(verdict, Decision::Ask(_)) {
            return Decision::Allow;
        }
        verdict
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::AutoApprove;

    fn denied(decision: &Decision) -> bool {
        matches!(decision, Decision::Deny(_))
    }

    #[tokio::test]
    async fn rule_priority_is_deny_then_ask_then_allow_regardless_of_registration() {
        let policy = RulePolicy::new(vec![
            Rule {
                matcher: RuleMatcher::Any,
                decision: Decision::Allow,
            },
            Rule {
                matcher: RuleMatcher::Tool("bash".into()),
                decision: Decision::Ask("first ask".into()),
            },
            Rule {
                matcher: RuleMatcher::Any,
                decision: Decision::Ask("second ask".into()),
            },
            Rule {
                matcher: RuleMatcher::CommandPrefix("rm".into()),
                decision: Decision::Deny("first deny".into()),
            },
            Rule {
                matcher: RuleMatcher::Tool("bash".into()),
                decision: Decision::Deny("second deny".into()),
            },
        ]);
        assert_eq!(
            policy.check("bash", &serde_json::json!({"command": "rm x"})).await,
            Decision::Deny("first deny".into())
        );
        assert_eq!(
            policy.check("bash", &serde_json::json!({"command": "ls"})).await,
            Decision::Deny("second deny".into())
        );
        assert_eq!(
            policy.check("read_file", &serde_json::json!({})).await,
            Decision::Ask("second ask".into())
        );
        let policy = RulePolicy::new(vec![
            Rule {
                matcher: RuleMatcher::Any,
                decision: Decision::Allow,
            },
            Rule {
                matcher: RuleMatcher::Tool("bash".into()),
                decision: Decision::Ask("first".into()),
            },
            Rule {
                matcher: RuleMatcher::Any,
                decision: Decision::Ask("second".into()),
            },
        ]);
        assert_eq!(
            policy.check("bash", &serde_json::json!({})).await,
            Decision::Ask("first".into())
        );
    }

    #[tokio::test]
    async fn unmatched_and_empty_prefix_rules_fail_closed() {
        let policy = RulePolicy::new(vec![
            Rule {
                matcher: RuleMatcher::CommandPrefix("".into()),
                decision: Decision::Allow,
            },
            Rule {
                matcher: RuleMatcher::CommandPrefix("ls".into()),
                decision: Decision::Allow,
            },
        ]);
        for (tool, args) in [
            ("bash", serde_json::json!({"command": "lsx"})),
            ("read_file", serde_json::json!({"command": "ls"})),
            ("bash", serde_json::json!({})),
        ] {
            assert!(matches!(policy.check(tool, &args).await, Decision::Deny(_)));
        }
        assert_eq!(
            policy.check("bash", &serde_json::json!({"command": "ls\t-a"})).await,
            Decision::Allow
        );
    }

    #[tokio::test]
    async fn auto_approval_turns_only_a_bash_ask_into_allow() {
        let policy = RulePolicy::new(vec![
            Rule {
                matcher: RuleMatcher::Tool("bash".into()),
                decision: Decision::Ask("approve bash".into()),
            },
            Rule {
                matcher: RuleMatcher::Any,
                decision: Decision::Ask("approve any".into()),
            },
        ])
        .auto_approve(AutoApprove::new(["ls"]));
        assert_eq!(
            policy.check("bash", &serde_json::json!({"command": "ls -a"})).await,
            Decision::Allow
        );
        assert_eq!(
            policy.check("bash", &serde_json::json!({"command": "rm x"})).await,
            Decision::Ask("approve bash".into())
        );
        assert_eq!(
            policy.check("bash", &serde_json::json!({})).await,
            Decision::Ask("approve bash".into())
        );
        assert_eq!(
            policy.check("read_file", &serde_json::json!({"command": "ls"})).await,
            Decision::Ask("approve any".into())
        );
    }

    #[tokio::test]
    async fn auto_approval_never_overrides_a_denial() {
        let list = AutoApprove::new(["ls"]);
        let ls = serde_json::json!({"command": "ls -a"});
        let denied_by_rule = RulePolicy::new(vec![Rule {
            matcher: RuleMatcher::CommandPrefix("ls".into()),
            decision: Decision::Deny("denied".into()),
        }])
        .auto_approve(list.clone());
        assert_eq!(denied_by_rule.check("bash", &ls).await, Decision::Deny("denied".into()));
        let unmatched = RulePolicy::new(Vec::new()).auto_approve(list);
        assert!(denied(&unmatched.check("bash", &ls).await));
    }

    #[tokio::test]
    async fn command_prefix_rules_compare_like_the_allowlist() {
        let deny = RulePolicy::new(vec![
            Rule {
                matcher: RuleMatcher::CommandPrefix("kubectl delete".into()),
                decision: Decision::Deny("no deletes".into()),
            },
            Rule {
                matcher: RuleMatcher::Any,
                decision: Decision::Allow,
            },
        ]);
        for command in ["kubectl  delete pod x", "KUBECTL delete pod x", "kubectl\tdelete pod x"] {
            assert_eq!(
                deny.check("bash", &serde_json::json!({ "command": command })).await,
                Decision::Deny("no deletes".into()),
                "{command}"
            );
        }
    }

    #[tokio::test]
    async fn an_allow_prefix_does_not_let_a_second_command_through() {
        let allow = RulePolicy::new(vec![Rule {
            matcher: RuleMatcher::CommandPrefix("kubectl get".into()),
            decision: Decision::Allow,
        }]);
        assert_eq!(
            allow
                .check("bash", &serde_json::json!({"command": "kubectl get pods -A"}))
                .await,
            Decision::Allow
        );
        for command in [
            "kubectl get pods ; rm -rf ~/scratch",
            "kubectl get pods && rm x",
            "kubectl get pods | sh",
            "kubectl get pods > /etc/hosts",
            "kubectl get $(rm x)",
            "kubectl get pods\nrm x",
        ] {
            assert!(
                denied(&allow.check("bash", &serde_json::json!({ "command": command })).await),
                "{command}"
            );
        }
    }
}
