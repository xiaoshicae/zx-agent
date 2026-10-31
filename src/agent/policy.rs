//! Tool-call policy (pi: `beforeToolCall`, wired to the extension `tool_call` event).
//!
//! An unattended run has no one to ask, so a policy decides on its own and should deny
//! when unsure. Policies are a first line of defense, not a security boundary: run the
//! agent as a least-privilege user, in a container or VM, for real isolation.

use async_trait::async_trait;
use serde_json::Value;

/// The verdict on one tool call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// The reason is shown to the model as the tool result.
    Deny(String),
}

/// Decides whether a tool call may run.
#[async_trait]
pub trait ToolPolicy: Send + Sync {
    /// Called after the tool is found and its arguments parsed, before it runs.
    async fn check(&self, tool_name: &str, args: &Value) -> Decision;
}

/// Policy that allows every call. For tests and fully trusted environments.
pub struct AllowAll;

#[async_trait]
impl ToolPolicy for AllowAll {
    async fn check(&self, _tool_name: &str, _args: &Value) -> Decision {
        Decision::Allow
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
            let allowed = self
                .allow_prefixes
                .iter()
                .any(|prefix| segment == prefix || segment.starts_with(&format!("{prefix} ")));
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
        if tool_name != "bash" {
            return Decision::Allow;
        }
        match args.get("command").and_then(Value::as_str) {
            Some(command) => self.evaluate(command),
            None => Decision::Deny("missing \"command\" argument".to_string()),
        }
    }
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
