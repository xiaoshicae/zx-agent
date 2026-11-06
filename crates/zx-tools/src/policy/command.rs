//! The command policy of the `bash` tool: denied patterns and programs, and the allowlist of
//! command prefixes.

use async_trait::async_trait;
use serde_json::Value;

use super::{begins_with, command_of, normalize};
use crate::agent::{PolicyDecision, ToolPolicy};
use crate::tools::BASH;

/// Patterns rejected for every `bash` command, compared as substrings after lowercasing and
/// collapsing whitespace. Easy to bypass on purpose; the allowlist is the real control.
pub const DEFAULT_DENIED_PATTERNS: &[&str] = &[
    "rm -rf /",
    "rm -fr /",
    "--no-preserve-root",
    "dd if=",
    ":(){",
    "> /dev/sd",
    "> /dev/nvme",
];

/// Programs rejected for every `bash` command, as is `init` or `telinit` followed by `0` or
/// `6`; `mkfs` also covers `mkfs.<type>`. A name counts as a word of a command, compared by
/// file name (`/sbin/reboot`) after quotes and backslashes are removed. The arguments of
/// read-only programs, such as `grep`, `last`, `journalctl`, `cat` and `echo`, do not count,
/// so `last reboot` or `grep -i shutdown` may run. Like the patterns, easy to bypass on
/// purpose.
pub const DENIED_PROGRAMS: &[&str] = &["shutdown", "reboot", "poweroff", "halt", "mkfs"];

/// Programs that cannot run another program: what follows them on the command line is data,
/// such as a pattern or a file name, never a command.
const READ_ONLY_PROGRAMS: &[&str] = &[
    "grep",
    "egrep",
    "fgrep",
    "zgrep",
    "rg",
    "last",
    "lastb",
    "journalctl",
    "echo",
    "printf",
    "cat",
    "zcat",
    "head",
    "tail",
    "wc",
    "sort",
    "uniq",
    "cut",
    "tr",
    "jq",
    "ls",
    "stat",
    "file",
    "man",
    "which",
    "whereis",
];

/// Programs that run the command after their options, looked through when deciding whether
/// a command is one of [`READ_ONLY_PROGRAMS`].
const WRAPPERS: &[&str] = &["sudo", "doas", "nice", "nohup", "time", "command", "exec", "env"];

/// Characters that end one command and start another, or start a command inside another.
const COMMAND_SEPARATORS: &[char] = &['|', ';', '&', '(', ')', '{', '}', '`', '\n', '\r'];

/// The first program of [`DENIED_PROGRAMS`] that `command` runs, as far as words can tell.
fn denied_program(command: &str) -> Option<String> {
    let lowered = command.to_lowercase();
    for part in lowered.split(COMMAND_SEPARATORS) {
        // Quotes and backslashes only change how the shell splits words, not their letters.
        let words: Vec<String> = part
            .split_whitespace()
            .map(|word| word.chars().filter(|c| !matches!(c, '"' | '\'' | '\\')).collect())
            .collect();
        let mut rest = words
            .iter()
            .map(String::as_str)
            .skip_while(|word| is_assignment(word))
            .map(file_name);
        let mut program = rest.next();
        while program.is_some_and(|name| WRAPPERS.contains(&name)) {
            program = rest.find(|word| !word.starts_with('-') && !is_assignment(word));
        }
        if program.is_some_and(|name| READ_ONLY_PROGRAMS.contains(&name)) {
            continue;
        }
        let names: Vec<&str> = words.iter().map(|word| file_name(word)).collect();
        for (index, name) in names.iter().enumerate() {
            let runlevel = matches!(*name, "init" | "telinit") && matches!(names.get(index + 1), Some(&"0" | &"6"));
            if DENIED_PROGRAMS.contains(name) || name.starts_with("mkfs.") || runlevel {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// The part of `word` after its last `/`.
fn file_name(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// Whether `word` sets a shell variable for the command, as in `LANG=C sort`.
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Shell syntax rejected when an allowlist is set: anything that could chain or redirect
/// beyond a plain pipeline of allowlisted programs.
pub(super) const ALLOWLIST_FORBIDDEN: &[&str] = &[";", "&", "`", "$(", ">", "<", "\n", "\r"];

/// Policy for the `bash` tool.
///
/// - Every command is checked against denied patterns and [`DENIED_PROGRAMS`].
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
                .map(|prefix| normalize(prefix.as_ref()))
                .filter(|prefix| !prefix.is_empty())
                .collect(),
            denied_patterns: DEFAULT_DENIED_PATTERNS
                .iter()
                .map(|pattern| normalize(pattern))
                .collect(),
        }
    }

    /// Add substrings to reject in every command.
    pub fn deny_also(mut self, patterns: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        self.denied_patterns
            .extend(patterns.into_iter().map(|pattern| normalize(pattern.as_ref())));
        self
    }

    /// Decide on one shell command.
    pub fn evaluate(&self, command: &str) -> PolicyDecision {
        let normalized = normalize(command);
        if let Some(pattern) = self
            .denied_patterns
            .iter()
            .find(|pattern| normalized.contains(pattern.as_str()))
        {
            return PolicyDecision::Deny(format!("command matches the denied pattern {pattern:?}"));
        }
        if let Some(program) = denied_program(command) {
            return PolicyDecision::Deny(format!("command runs the denied program {program:?}"));
        }
        if self.allow_prefixes.is_empty() {
            return PolicyDecision::Allow;
        }
        // Check the raw command too: normalization turns newlines into spaces.
        if let Some(token) = ALLOWLIST_FORBIDDEN.iter().find(|token| command.contains(*token)) {
            return PolicyDecision::Deny(format!(
                "{token:?} is not allowed; only plain commands and '|' pipelines of allowlisted programs may run"
            ));
        }
        for segment in normalized.split('|') {
            let segment = segment.trim();
            if segment.is_empty() {
                return PolicyDecision::Deny("empty pipeline segment (is '||' used?)".to_string());
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
                return PolicyDecision::Deny(format!(
                    "{segment:?} is not in the command allowlist ({}){hint}",
                    self.allow_prefixes.join(", ")
                ));
            }
        }
        PolicyDecision::Allow
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
    async fn check(&self, tool_name: &str, args: &Value) -> PolicyDecision {
        if tool_name != BASH {
            return PolicyDecision::Allow;
        }
        match command_of(args) {
            Some(command) => self.evaluate(command),
            None => PolicyDecision::Deny("missing \"command\" argument".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn denied(decision: &PolicyDecision) -> bool {
        matches!(decision, PolicyDecision::Deny(_))
    }

    #[test]
    fn denied_patterns_apply_without_allowlist() {
        let policy = CommandPolicy::new(Vec::<String>::new());
        assert_eq!(policy.evaluate("df -h"), PolicyDecision::Allow);
        assert!(denied(&policy.evaluate("sudo  RM -RF  /")));
        assert!(denied(&policy.evaluate("systemctl reboot")));
    }

    #[test]
    fn allowlist_accepts_pipelines_of_allowed_programs() {
        let policy = CommandPolicy::new(["kubectl get", "grep", "journalctl", "tail"]);
        assert_eq!(
            policy.evaluate("kubectl get pods -A | grep -v Running"),
            PolicyDecision::Allow
        );
        assert_eq!(
            policy.evaluate("journalctl -u nginx --since '1 hour ago' | tail -n 50"),
            PolicyDecision::Allow
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
        assert_eq!(policy.evaluate("kubectl get pods"), PolicyDecision::Allow);
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
        let PolicyDecision::Deny(reason) = policy.evaluate("grep -n \"a|b\" file") else {
            panic!("a quoted pipe must not pass the allowlist");
        };
        assert!(reason.contains("grep -e A -e B"), "{reason}");
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
            assert_eq!(policy.evaluate(command), PolicyDecision::Allow, "{command}");
        }
    }

    #[tokio::test]
    async fn only_bash_is_checked() {
        let policy = CommandPolicy::new(["ls"]);
        assert_eq!(
            policy
                .check("read_file", &serde_json::json!({"path": "/etc/hosts"}))
                .await,
            PolicyDecision::Allow
        );
        assert!(denied(&policy.check("bash", &serde_json::json!({})).await));
    }
}
