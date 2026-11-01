//! Host policy implementations; the loop sees them only through Hooks.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::agent::{Decision, HookResult, Hooks, ToolPolicy};
use crate::ai::ToolCall;
use crate::tools::BASH;

/// Adapts an existing asynchronous policy to the bounded before_tool hook.
pub struct PolicyHook {
    policy: Arc<dyn ToolPolicy>,
}

impl PolicyHook {
    pub fn new(policy: Arc<dyn ToolPolicy>) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl Hooks for PolicyHook {
    async fn before_tool(&self, call: &ToolCall) -> HookResult<Decision> {
        Ok(self.policy.check(&call.name, &call.arguments).await)
    }
}

/// A lexical match, not a shell parser or an execution sandbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuleMatcher {
    Any,
    Tool(String),
    /// Matches bash's command at a whitespace boundary. Use CommandPolicy as well
    /// when shell syntax must be restricted.
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
                if tool != BASH || prefix.trim().is_empty() {
                    return false;
                }
                args.get("command").and_then(Value::as_str).is_some_and(|command| {
                    let command = command.trim_start();
                    command == prefix
                        || command
                            .strip_prefix(prefix)
                            .is_some_and(|suffix| suffix.starts_with(char::is_whitespace))
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

    fn rule_verdict(&self, tool_name: &str, args: &Value) -> Decision {
        for priority in 0..3 {
            for rule in &self.rules {
                let rank = match rule.decision {
                    Decision::Deny(_) => 0,
                    Decision::Ask(_) => 1,
                    Decision::Allow => 2,
                };
                if rank == priority && rule.matches(tool_name, args) {
                    return rule.decision.clone();
                }
            }
        }
        Decision::Deny(format!("no policy rule permits {tool_name:?}"))
    }
}

#[async_trait]
impl ToolPolicy for RulePolicy {
    async fn check(&self, tool_name: &str, args: &Value) -> Decision {
        let verdict = self.rule_verdict(tool_name, args);
        let covered = tool_name == BASH
            && args
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(|command| self.auto_approve.covers(command));
        if covered && matches!(verdict, Decision::Ask(_)) {
            return Decision::Allow;
        }
        verdict
    }
}

/// Patterns rejected for every `bash` command, compared after lowercasing and
/// collapsing whitespace. Easy to bypass on purpose; the allowlist is the real control.
pub const DEFAULT_DENIED_PATTERNS: &[&str] = &[
    "rm -rf /",
    "rm -fr /",
    "--no-preserve-root",
    "mkfs",
    "dd if=",
    ":(){",
    "shutdown",
    "reboot",
    "poweroff",
    "halt",
    "init 0",
    "init 6",
    "> /dev/sd",
    "> /dev/nvme",
];

/// Shell syntax rejected when an allowlist is set: anything that could chain or redirect
/// beyond a plain pipeline of allowlisted programs.
const ALLOWLIST_FORBIDDEN: &[&str] = &[";", "&", "`", "$(", ">", "<", "\n", "\r"];

/// Policy for the `bash` tool.
///
/// - Every command is checked against denied patterns.
/// - With an empty allowlist, other commands are allowed.
/// - With an allowlist, a command must be a pipeline (`a | b`) whose every segment starts
///   with an allowlisted prefix, such as `kubectl get` or `journalctl`. Other shell
///   control syntax is rejected, so the check fails closed on anything it cannot read.
pub struct CommandPolicy {
    allow_prefixes: Vec<String>,
    denied_patterns: Vec<String>,
}

impl CommandPolicy {
    /// Create a policy with the default denied patterns. An empty `allow_prefixes` allows
    /// every command that does not match a denied pattern.
    pub fn new(allow_prefixes: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        Self {
            allow_prefixes: allow_prefixes
                .into_iter()
                .map(|p| normalize(p.as_ref()))
                .filter(|p| !p.is_empty())
                .collect(),
            denied_patterns: DEFAULT_DENIED_PATTERNS.iter().map(|p| normalize(p)).collect(),
        }
    }

    /// Add substrings to reject in every command.
    pub fn deny_also(mut self, patterns: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        self.denied_patterns
            .extend(patterns.into_iter().map(|p| normalize(p.as_ref())));
        self
    }

    /// Decide on one shell command.
    pub fn evaluate(&self, command: &str) -> Decision {
        let normalized = normalize(command);
        if let Some(pattern) = self.denied_patterns.iter().find(|p| normalized.contains(p.as_str())) {
            return Decision::Deny(format!("command matches the denied pattern {pattern:?}"));
        }
        if self.allow_prefixes.is_empty() {
            return Decision::Allow;
        }
        // Check the raw command too: normalization turns newlines into spaces.
        if let Some(token) = ALLOWLIST_FORBIDDEN.iter().find(|t| command.contains(*t)) {
            return Decision::Deny(format!(
                "{token:?} is not allowed; only plain commands and '|' pipelines of allowlisted programs may run"
            ));
        }
        for segment in normalized.split('|') {
            let segment = segment.trim();
            if segment.is_empty() {
                return Decision::Deny("empty pipeline segment (is '||' used?)".to_string());
            }
            let allowed = self.allow_prefixes.iter().any(|prefix| begins_with(segment, prefix));
            if !allowed {
                // An odd number of quotes means the split happened inside a quoted string.
                let split_quote = segment.matches('"').count() % 2 == 1 || segment.matches('\'').count() % 2 == 1;
                let hint = if split_quote {
                    "; a '|' separates commands even inside quotes, so write grep -e A -e B instead of grep \"A|B\""
                } else {
                    ""
                };
                return Decision::Deny(format!(
                    "{segment:?} is not in the command allowlist ({}){hint}",
                    self.allow_prefixes.join(", ")
                ));
            }
        }
        Decision::Allow
    }

    /// The rules for the system prompt, so the model knows them before its first command
    /// instead of learning them from rejections. Built from the same data as `evaluate`.
    /// `None` without an allowlist.
    pub fn prompt_section(&self) -> Option<String> {
        if self.allow_prefixes.is_empty() {
            return None;
        }
        let symbols: Vec<&str> = ALLOWLIST_FORBIDDEN
            .iter()
            .copied()
            .filter(|token| !token.trim().is_empty())
            .collect();
        Some(format!(
            "bash commands are checked before they run; rejected commands return an error and do not run.\n\
             Every command, and every segment of a '|' pipeline, must start with one of: {}.\n\
             Rejected syntax: {} and line breaks.\n\
             A '|' separates commands even inside quotes: write grep -e A -e B instead of grep \"A|B\".",
            self.allow_prefixes.join(", "),
            symbols.join(" "),
        ))
    }
}

#[async_trait]
impl ToolPolicy for CommandPolicy {
    async fn check(&self, tool_name: &str, args: &Value) -> Decision {
        if tool_name != BASH {
            return Decision::Allow;
        }
        match args.get("command").and_then(Value::as_str) {
            Some(command) => self.evaluate(command),
            None => Decision::Deny("missing \"command\" argument".to_string()),
        }
    }
}

/// Options that send a request, and the credentials attached to it, to a server other than
/// the configured one. kubectl v1.36.1 sent the kubeconfig token to an HTTPS address given
/// with `--server` (docs/p6-online-access-review.md, appendix 9.3, K1).
const CONNECTION_OPTIONS: &[&str] = &["--server", "--kubeconfig", "--token", "--insecure-skip-tls-verify"];

/// Bash commands that run without asking where a rule would otherwise ask for approval.
///
/// A command is covered only when it is a plain `|` pipeline whose every segment starts with
/// a listed prefix, compared like the allowlist (whitespace collapsed, lowercased), and it
/// contains none of these: the syntax the allowlist rejects, the word `secret`, a URL
/// (`://`), a connection option (`--server`, `--kubeconfig`, `--token`,
/// `--insecure-skip-tls-verify`), or, in a kubectl segment, a short option containing `s`,
/// which covers `-s` in every spelling. Like the allowlist the check is lexical: it decides
/// only whether to ask, and never turns a denial into an approval.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AutoApprove {
    prefixes: Vec<String>,
}

impl AutoApprove {
    /// Commands starting with one of `prefixes`, such as `kubectl get`. An empty list covers
    /// no command.
    pub fn new(prefixes: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        Self {
            prefixes: prefixes
                .into_iter()
                .map(|p| normalize(p.as_ref()))
                .filter(|p| !p.is_empty())
                .collect(),
        }
    }

    /// Whether `command` may run without asking.
    pub fn covers(&self, command: &str) -> bool {
        if self.prefixes.is_empty() || ALLOWLIST_FORBIDDEN.iter().any(|token| command.contains(token)) {
            return false;
        }
        let normalized = normalize(command);
        if normalized.contains("secret") || normalized.contains("://") {
            return false;
        }
        normalized
            .split('|')
            .map(str::trim)
            .all(|segment| self.prefixes.iter().any(|prefix| begins_with(segment, prefix)) && !redirects(segment))
    }
}

/// Whether a normalized pipeline segment passes an option that can send its request elsewhere.
fn redirects(segment: &str) -> bool {
    let kubectl = segment.split(' ').next() == Some("kubectl");
    segment.split(' ').any(|word| {
        CONNECTION_OPTIONS.iter().any(|option| word.starts_with(option))
            // Only for kubectl: elsewhere short options mean other things (jq -s, grep -s).
            || (kubectl && word.starts_with('-') && !word.starts_with("--") && word.contains('s'))
    })
}

/// Whether a normalized pipeline segment starts with `prefix` at a word boundary.
fn begins_with(segment: &str, prefix: &str) -> bool {
    segment
        .strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
}

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn denied(decision: Decision) -> bool {
        matches!(decision, Decision::Deny(_))
    }

    #[test]
    fn denied_patterns_apply_without_allowlist() {
        let policy = CommandPolicy::new(Vec::<String>::new());
        assert_eq!(policy.evaluate("df -h"), Decision::Allow);
        assert!(denied(policy.evaluate("sudo  RM -RF  /")));
        assert!(denied(policy.evaluate("systemctl reboot")));
    }

    #[test]
    fn allowlist_accepts_pipelines_of_allowed_programs() {
        let policy = CommandPolicy::new(["kubectl get", "grep", "journalctl", "tail"]);
        assert_eq!(
            policy.evaluate("kubectl get pods -A | grep -v Running"),
            Decision::Allow
        );
        assert_eq!(
            policy.evaluate("journalctl -u nginx --since '1 hour ago' | tail -n 50"),
            Decision::Allow
        );
        assert!(denied(policy.evaluate("kubectl delete pod x")));
        assert!(denied(policy.evaluate("kubectl get pods; rm x")));
        assert!(denied(policy.evaluate("kubectl get pods && rm x")));
        assert!(denied(policy.evaluate("grep x $(cat list)")));
        assert!(denied(policy.evaluate("tail -n 5 log > out")));
        assert!(denied(policy.evaluate("grep a || rm b")));
        assert!(denied(policy.evaluate("grep a\nrm b")));
        assert!(denied(policy.evaluate("grepx a")));
    }

    #[test]
    fn extra_denied_patterns_apply_to_every_command() {
        let policy = CommandPolicy::new(Vec::<String>::new()).deny_also(["kubectl delete"]);
        assert!(denied(policy.evaluate("KUBECTL  delete pod x")));
        assert_eq!(policy.evaluate("kubectl get pods"), Decision::Allow);
    }

    #[test]
    fn describes_the_allowlist_for_the_prompt() {
        assert_eq!(CommandPolicy::new(Vec::<String>::new()).prompt_section(), None);
        let section = CommandPolicy::new(["kubectl get", "grep"]).prompt_section().unwrap();
        assert!(
            section.contains("must start with one of: kubectl get, grep."),
            "{section}"
        );
        assert!(
            section.contains("Rejected syntax: ; & ` $( > < and line breaks."),
            "{section}"
        );
    }

    #[test]
    fn denial_of_a_quoted_pipe_suggests_an_alternative() {
        let policy = CommandPolicy::new(["grep"]);
        let Decision::Deny(reason) = policy.evaluate("grep -n \"a|b\" file") else {
            panic!("a quoted pipe must not pass the allowlist");
        };
        assert!(reason.contains("grep -e A -e B"), "{reason}");
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

    #[test]
    fn auto_approval_covers_listed_read_only_pipelines() {
        let list = AutoApprove::new(["kubectl get", "grep", "jq"]);
        for command in [
            "kubectl get pods -A | grep -v Running",
            "KUBECTL  get\tpods -n prod",
            "kubectl get pods -o json | jq -s .",
            "grep -s error app.log",
        ] {
            assert!(list.covers(command), "{command}");
        }
    }

    #[test]
    fn auto_approval_asks_for_commands_beyond_the_list() {
        let list = AutoApprove::new(["kubectl get", "grep"]);
        for command in [
            "kubectl delete pod x",
            "kubectl get pods | kubectl delete pod x",
            "grepx error",
            "grep a || grep b",
            "kubectl get pods; rm x",
            "kubectl get pods && rm x",
            "grep x $(cat list)",
            "grep x app.log > copy",
            "grep a\nrm b",
        ] {
            assert!(!list.covers(command), "{command}");
        }
        assert!(!AutoApprove::default().covers("grep x"));
    }

    #[test]
    fn auto_approval_asks_for_secrets_urls_and_connection_options() {
        let list = AutoApprove::new(["kubectl get", "grep"]);
        for command in [
            "kubectl get secret db -o yaml",
            "kubectl get SECRETS -A",
            "kubectl get -f https://example.com/pods.yaml",
            "kubectl get pods --server=https://example.com",
            "kubectl get pods --server example.com:443",
            "kubectl get pods -s example.com:443",
            "kubectl get pods -sexample.com:443",
            "kubectl get pods -As example.com:443",
            "kubectl get pods --kubeconfig /tmp/other",
            "kubectl get pods --token abc",
            "kubectl get pods --insecure-skip-tls-verify",
            "grep --server x app.log",
        ] {
            assert!(!list.covers(command), "{command}");
        }
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
        assert!(denied(unmatched.check("bash", &ls).await));
    }

    #[tokio::test]
    async fn only_bash_is_checked() {
        let policy = CommandPolicy::new(["ls"]);
        assert_eq!(
            policy
                .check("read_file", &serde_json::json!({"path": "/etc/hosts"}))
                .await,
            Decision::Allow
        );
        assert!(denied(policy.check("bash", &serde_json::json!({})).await));
    }
}
