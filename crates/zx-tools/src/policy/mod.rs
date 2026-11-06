//! The built-in tool-call policies: rules by tool or command prefix, the command policy of
//! the `bash` tool, and the auto-approval of plain commands; the loop sees them only through
//! Hooks.

mod auto_approve;
mod command;
mod rules;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

pub use auto_approve::{AutoApprove, ProgramOptions};
pub use command::{CommandPolicy, DEFAULT_DENIED_PATTERNS, DENIED_PROGRAMS};
pub use rules::{Rule, RuleMatcher, RulePolicy};

use crate::agent::{Decision, HookContext, HookResult, HookStage, Hooks, ToolPolicy};
use crate::ai::ToolCall;

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
    /// Before any extension sees the call.
    fn stage(&self) -> HookStage {
        HookStage::Policy
    }

    async fn before_tool(&self, call: &ToolCall, _ctx: &HookContext) -> HookResult<Decision> {
        Ok(self.policy.check(&call.name, &call.arguments).await)
    }
}

/// The `command` argument of a bash call, when it is a string.
fn command_of(args: &Value) -> Option<&str> {
    args.get("command").and_then(Value::as_str)
}

/// Whether a normalized pipeline segment starts with `prefix` at a word boundary.
pub(super) fn begins_with(segment: &str, prefix: &str) -> bool {
    segment
        .strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
}

pub(super) fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

pub(super) fn normalize_all(texts: impl IntoIterator<Item = impl AsRef<str>>) -> Vec<String> {
    texts
        .into_iter()
        .map(|text| normalize(text.as_ref()))
        .filter(|text| !text.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn denied(decision: &Decision) -> bool {
        matches!(decision, Decision::Deny(_))
    }

    #[test]
    fn denied_patterns_apply_without_allowlist() {
        let policy = CommandPolicy::new(Vec::<String>::new());
        assert_eq!(policy.evaluate("df -h"), Decision::Allow);
        assert!(denied(&policy.evaluate("sudo  RM -RF  /")));
        assert!(denied(&policy.evaluate("systemctl reboot")));
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
        assert!(denied(&policy.evaluate("kubectl delete pod x")));
        assert!(denied(&policy.evaluate("kubectl get pods; rm x")));
        assert!(denied(&policy.evaluate("kubectl get pods && rm x")));
        assert!(denied(&policy.evaluate("grep x $(cat list)")));
        assert!(denied(&policy.evaluate("tail -n 5 log > out")));
        assert!(denied(&policy.evaluate("grep a || rm b")));
        assert!(denied(&policy.evaluate("grep a\nrm b")));
        assert!(denied(&policy.evaluate("grepx a")));
    }

    #[test]
    fn extra_denied_patterns_apply_to_every_command() {
        let policy = CommandPolicy::new(Vec::<String>::new()).deny_also(["kubectl delete"]);
        assert!(denied(&policy.evaluate("KUBECTL  delete pod x")));
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

    /// Read-only kubectl options, as a deployment's profile lists them; this crate knows none.
    fn kubectl_read_only() -> ProgramOptions {
        ProgramOptions {
            long: [
                "namespace",
                "context",
                "all-namespaces",
                "output",
                "selector",
                "field-selector",
                "sort-by",
                "container",
                "previous",
                "since",
                "tail",
            ]
            .map(String::from)
            .to_vec(),
            short_with_value: vec!['n', 'o', 'l', 'c'],
            short_flags: vec!['A', 'p'],
        }
    }

    fn kubectl_list(prefixes: &[&str]) -> AutoApprove {
        AutoApprove::new(prefixes)
            .except(["secret"])
            .options("kubectl", kubectl_read_only())
    }

    #[test]
    fn auto_approval_asks_for_urls_listed_texts_and_unlisted_options() {
        let list = kubectl_list(&["kubectl get", "grep"]);
        for command in [
            "kubectl get secret db -o yaml",
            "kubectl get SECRETS -A",
            "kubectl get -f https://example.com/pods.yaml",
            "grep -r token https://example.com",
            "kubectl get pods --server=https://example.com",
            "kubectl get pods --server example.com:443",
            "kubectl get pods -s example.com:443",
            "kubectl get pods -sexample.com:443",
            "kubectl get pods -As example.com:443",
            "kubectl get pods --kubeconfig /tmp/other",
            "kubectl get pods --token abc",
            "kubectl get pods --insecure-skip-tls-verify",
        ] {
            assert!(!list.covers(command), "{command}");
        }
    }

    #[test]
    fn a_program_given_no_options_passes_any_option() {
        // kubectl's options limit kubectl only.
        assert!(kubectl_list(&["kubectl get", "grep"]).covers("grep --server x app.log"));
        assert!(AutoApprove::new(["kubectl get"]).covers("kubectl get pods --raw /api"));
        assert!(!kubectl_list(&["kubectl get"]).covers("kubectl get pods --raw /api"));
    }

    #[test]
    fn options_given_again_for_a_program_replace_the_earlier_ones() {
        let list = kubectl_list(&["kubectl get"]).options(
            "KUBECTL",
            ProgramOptions {
                long: vec!["raw".into()],
                ..ProgramOptions::default()
            },
        );
        assert!(list.covers("kubectl get --raw /api"));
        assert!(!list.covers("kubectl get pods -n prod"));
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

    #[test]
    fn auto_approval_asks_when_quoting_or_expansion_could_change_a_word() {
        let list = AutoApprove::new(["kubectl get", "grep"]);
        for command in [
            "kubectl get sec\"\"rets -A -o yaml",
            "kubectl get sec''rets -A -o yaml",
            "kubectl get sec\\rets -A -o yaml",
            "kubectl get sec${X}rets -A -o yaml",
            "kubectl get s?crets -A",
            "kubectl get s[e]crets -A",
            "kubectl get sec{r,}ets -A",
            "kubectl get pods \"--server=evil.example:443\"",
            "kubectl get pods --ser''ver=evil.example:443",
            "kubectl get pods \"-s\" evil.example:443",
            "kubectl get --raw \"https:\"//evil.example/x",
            "grep error app.log # note",
        ] {
            assert!(!list.covers(command), "{command}");
        }
    }

    #[test]
    fn auto_approval_asks_for_options_the_program_does_not_list() {
        let list = kubectl_list(&["kubectl get", "kubectl version", "kubectl logs"]);
        for command in [
            "kubectl version --client --profile=heap --profile-output=/home/u/.bashrc",
            "kubectl get pods --cache-dir=/home/u/.kube",
            "kubectl get --raw /api/v1/namespaces/default/pods",
            "kubectl get pods --context remote --user other-admin",
            "kubectl get pods --cluster other",
            "kubectl get pods --as=system:admin",
            "kubectl get pods -v=9",
            "kubectl get -f pods.yaml",
            "kubectl get pods -w",
            "kubectl get pods -Ap",
            "kubectl logs web --insecure-skip-tls-verify-backend",
        ] {
            assert!(!list.covers(command), "{command}");
        }
    }

    #[test]
    fn auto_approval_covers_options_the_program_lists() {
        let list = kubectl_list(&["kubectl get", "kubectl logs", "kubectl top", "grep"]);
        for command in [
            "kubectl get pods -n kube-system -o wide",
            "kubectl get pods -nkube-system -oyaml",
            "kubectl get pods --namespace=prod --context=remote --sort-by=.metadata.name",
            "kubectl get pods -A -l app=web,tier=api --field-selector=status.phase!=Running",
            "kubectl logs web-1 -c app --previous --tail=200 --since=1h",
            "kubectl top pods -A --sort-by=cpu",
            "grep -i ^error ~/logs/app.log",
        ] {
            assert!(list.covers(command), "{command}");
        }
    }

    #[test]
    fn power_programs_are_denied_wherever_a_command_starts() {
        let policy = CommandPolicy::new(Vec::<String>::new());
        for command in [
            "reboot",
            "sudo reboot",
            "sudo -u root reboot",
            "/sbin/re''boot",
            "sudo systemctl re\\boot",
            "systemctl --no-wall poweroff",
            "shutdown -h now",
            "timeout -s KILL 5 halt",
            "find / -exec reboot \\;",
            "xargs reboot",
            "bash -c \"reboot\"",
            "init 6",
            "sudo telinit 0",
            "mkfs.ext4 /dev/sdb1",
            "uptime; reboot",
            "uptime && reboot",
            "uptime | poweroff",
            "echo `reboot`",
            "echo $(reboot)",
            "(reboot)",
            "{ reboot; }",
            "uptime\nreboot",
        ] {
            assert!(denied(&policy.evaluate(command)), "{command}");
        }
    }

    #[test]
    fn power_words_in_arguments_of_read_only_programs_are_allowed() {
        let policy = CommandPolicy::new(Vec::<String>::new());
        for command in [
            "kubectl get pods -n asphalt-prod",
            "last reboot",
            "journalctl -b -1 --no-pager | grep -i reboot | tail -n 20",
            "kubectl get events -A | grep -i shutdown",
            "grep -c 'graceful shutdown' /var/log/app.log",
            "sudo grep -i reboot /var/log/syslog",
            "LANG=C env TZ=UTC grep -c shutdown /var/log/syslog",
            "systemctl status reboot.target",
            "kubectl logs reboot-job-1",
        ] {
            assert_eq!(policy.evaluate(command), Decision::Allow, "{command}");
        }
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

    #[tokio::test]
    async fn only_bash_is_checked() {
        let policy = CommandPolicy::new(["ls"]);
        assert_eq!(
            policy
                .check("read_file", &serde_json::json!({"path": "/etc/hosts"}))
                .await,
            Decision::Allow
        );
        assert!(denied(&policy.check("bash", &serde_json::json!({})).await));
    }
}
