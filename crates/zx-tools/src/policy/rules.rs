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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub matcher: RuleMatcher,
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
