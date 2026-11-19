//! The command policy of the `bash` tool: denied patterns and programs.

use async_trait::async_trait;

use super::{BashCall, bash_call, non_empty, normalize};
use crate::ai::ToolCall;
use zx_core::policy::{PolicyContext, PolicyDecision, ToolPolicy};

/// Patterns rejected for every `bash` command, compared as substrings after lowercasing and
/// collapsing whitespace. Easy to bypass on purpose: a denylist only stops the plain spelling,
/// so commands that must not run need a rule that asks, or isolation. `rm -rf /` and
/// `rm -fr /` are rejected too, by a rule of their own: only where the `/` is the whole path.
pub const DEFAULT_DENIED_PATTERNS: &[&str] = &["--no-preserve-root", "dd if=", ":(){", "> /dev/sd", "> /dev/nvme"];

/// The spellings of a forced, recursive removal of the root directory, rejected for every
/// `bash` command where the `/` is the whole path (see [`AFTER_ROOT`]). They are not among
/// [`DEFAULT_DENIED_PATTERNS`]: as substrings they would also reject the removal of any
/// absolute path, such as `rm -rf /tmp/build`.
const ROOT_REMOVALS: &[&str] = &["rm -rf /", "rm -fr /"];

/// What may follow the `/` of a root removal, besides the end of the command, for the `/` to
/// be the whole path: whitespace, `*` (everything in the root), and the characters that end
/// a word or a command, as in `bash -c "rm -rf /"` and `rm -rf /; true`. Anything else goes
/// on with a path below the root.
const AFTER_ROOT: &[char] = &[' ', '*', '"', '\'', ';', '&', '|', ')', '`'];

/// Programs rejected for every `bash` command by default, as is `init` or `telinit` followed
/// by `0` or `6`; `mkfs` also covers `mkfs.<type>`. A name counts as a word of a command,
/// compared by file name (`/sbin/reboot`) after quotes and backslashes are removed. The
/// arguments of read-only programs ([`DEFAULT_READ_ONLY_PROGRAMS`]) do not count, so
/// `last reboot` or `grep -i shutdown` may run. Like the patterns, easy to bypass on purpose.
pub const DEFAULT_DENIED_PROGRAMS: &[&str] = &["shutdown", "reboot", "poweroff", "halt", "mkfs"];

/// Programs that cannot run another program, by default: what follows them on the command
/// line is data, such as a pattern or a file name, never a command, so a denied program named
/// there does not count. A program with an option that names a program to run is not one of
/// them, however much it otherwise only reads: `rg` (`--pre`), `man` (`-P`) and `sort`
/// (`--compress-program`).
pub const DEFAULT_READ_ONLY_PROGRAMS: &[&str] = &[
    "grep",
    "egrep",
    "fgrep",
    "zgrep",
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
    "uniq",
    "cut",
    "tr",
    "jq",
    "ls",
    "stat",
    "file",
    "which",
    "whereis",
];

/// Programs that run the command after their options, looked through when deciding whether
/// a command is one of the read-only programs.
const WRAPPERS: &[&str] = &["sudo", "doas", "nice", "nohup", "time", "command", "exec", "env"];

/// Characters that end one command and start another, or start a command inside another.
const COMMAND_SEPARATORS: &[char] = &['|', ';', '&', '(', ')', '{', '}', '`', '\n', '\r'];

/// The part of `word` after its last `/`.
fn file_name(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// `text` as a program name is compared: trimmed, by file name and lowercased.
fn program_name(text: &str) -> String {
    file_name(text.trim()).to_lowercase()
}

/// Whether `word` sets a shell variable for the command, as in `LANG=C sort`.
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// The spelling of [`ROOT_REMOVALS`] that `normalized`, a command lowercased and with its
/// whitespace collapsed, contains with nothing of a path after its `/`.
fn root_removal(normalized: &str) -> Option<&'static str> {
    ROOT_REMOVALS.iter().copied().find(|spelling| {
        // Every piece after the first starts right behind one occurrence of the spelling.
        normalized
            .split(spelling)
            .skip(1)
            .any(|rest| rest.chars().next().is_none_or(|next| AFTER_ROOT.contains(&next)))
    })
}

/// Policy for the `bash` tool: every command is checked against denied patterns, the
/// removal of the root directory and denied programs; other commands are allowed. The
/// patterns and programs are those of [`DEFAULT_DENIED_PATTERNS`] and
/// [`DEFAULT_DENIED_PROGRAMS`], and whatever a host adds, which knows its own environment.
pub struct CommandPolicy {
    denied_patterns: Vec<String>,
    /// Program names, lowercased.
    denied_programs: Vec<String>,
    /// Program names, lowercased.
    read_only_programs: Vec<String>,
}

impl Default for CommandPolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandPolicy {
    /// A policy with the default denied patterns, denied programs and read-only programs.
    pub fn new() -> Self {
        Self {
            denied_patterns: DEFAULT_DENIED_PATTERNS
                .iter()
                .map(|pattern| normalize(pattern))
                .collect(),
            denied_programs: non_empty(DEFAULT_DENIED_PROGRAMS, program_name),
            read_only_programs: non_empty(DEFAULT_READ_ONLY_PROGRAMS, program_name),
        }
    }

    /// Add substrings to reject in every command, compared like the default ones, lowercased
    /// and with each run of whitespace as one space; empty ones are ignored, since every
    /// command holds the empty text.
    pub fn deny_also(mut self, patterns: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        self.denied_patterns.extend(non_empty(patterns, normalize));
        self
    }

    /// Add programs to reject wherever a command runs them, compared like the default ones:
    /// as one word, by file name and without regard to case; empty names are ignored. A name
    /// of several words never matches; reject such a text with [`deny_also`](Self::deny_also).
    pub fn deny_programs_also(mut self, programs: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        self.denied_programs.extend(non_empty(programs, program_name));
        self
    }

    /// Add programs whose arguments are data, so that a denied program named there does not
    /// count, compared like the default ones. Only for programs that cannot run another
    /// program: one that can, for example through an option, would let a denied program run.
    pub fn read_only_programs_also(mut self, programs: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        self.read_only_programs.extend(non_empty(programs, program_name));
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
        if let Some(spelling) = root_removal(&normalized) {
            return PolicyDecision::Deny(format!("command matches the denied pattern {spelling:?}"));
        }
        if let Some(program) = self.denied_program(command) {
            return PolicyDecision::Deny(format!("command runs the denied program {program:?}"));
        }
        PolicyDecision::Allow
    }

    /// The decision on `call`: every tool but `bash` is allowed, and a bash call without a
    /// command is denied.
    pub fn decision(&self, call: &ToolCall) -> PolicyDecision {
        match bash_call(call) {
            BashCall::Other => PolicyDecision::Allow,
            BashCall::NoCommand => PolicyDecision::Deny("missing \"command\" argument".to_string()),
            BashCall::Command(command) => self.evaluate(command),
        }
    }

    /// The first denied program that `command` runs, as far as words can tell.
    fn denied_program(&self, command: &str) -> Option<String> {
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
            if program.is_some_and(|name| self.read_only_programs.iter().any(|listed| listed == name)) {
                continue;
            }
            let names: Vec<&str> = words.iter().map(|word| file_name(word)).collect();
            for (index, name) in names.iter().enumerate() {
                let runlevel = matches!(*name, "init" | "telinit") && matches!(names.get(index + 1), Some(&"0" | &"6"));
                let denied = self.denied_programs.iter().any(|listed| listed == name);
                if denied || name.starts_with("mkfs.") || runlevel {
                    return Some(name.to_string());
                }
            }
        }
        None
    }
}

#[async_trait]
impl ToolPolicy for CommandPolicy {
    async fn check(&self, call: &ToolCall, _ctx: &PolicyContext) -> PolicyDecision {
        self.decision(call)
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
    fn denied_patterns_apply_and_other_commands_run() {
        let policy = CommandPolicy::new();
        assert_eq!(policy.evaluate("df -h"), PolicyDecision::Allow);
        assert!(denied(&policy.evaluate("sudo  RM -RF  /")));
        assert!(denied(&policy.evaluate("systemctl reboot")));
    }

    #[test]
    fn an_empty_denied_pattern_denies_nothing() {
        let policy = CommandPolicy::new().deny_also(["", "  "]);
        assert_eq!(policy.evaluate("df -h"), PolicyDecision::Allow);
    }

    #[test]
    fn removing_the_root_is_denied_wherever_its_slash_ends_the_path() {
        let policy = CommandPolicy::new();
        for command in [
            "rm -rf /",
            "rm -rf /*",
            "rm -rf / ",
            "rm -fr /",
            "rm -rf / /tmp/build",
            "rm -rf /tmp/build && rm -rf /",
            "rm -rf /; echo done",
            "bash -c \"rm -rf /\"",
            "ssh host 'rm -fr /'",
            "(cd /srv && rm -rf /)",
        ] {
            assert!(denied(&policy.evaluate(command)), "{command}");
        }
    }

    #[test]
    fn removing_a_path_below_the_root_is_allowed() {
        let policy = CommandPolicy::new();
        for command in [
            "rm -rf /tmp/build",
            "rm -fr /var/cache/app",
            "rm -rf /tmp/build /tmp/out",
            "cd /srv/app && rm -rf /srv/app/target",
        ] {
            assert_eq!(policy.evaluate(command), PolicyDecision::Allow, "{command}");
        }
    }

    #[test]
    fn programs_that_can_run_another_are_not_taken_to_only_read() {
        let policy = CommandPolicy::new();
        for command in [
            "rg --pre reboot pattern",
            "man -P reboot ls",
            "sort --compress-program reboot big.log",
        ] {
            assert!(denied(&policy.evaluate(command)), "{command}");
        }
    }

    #[test]
    fn extra_denied_patterns_apply_to_every_command() {
        let policy = CommandPolicy::new().deny_also(["kubectl delete"]);
        assert!(denied(&policy.evaluate("KUBECTL  delete pod x")));
        assert_eq!(policy.evaluate("kubectl get pods"), PolicyDecision::Allow);
    }

    #[test]
    fn extra_denied_programs_apply_like_the_default_ones() {
        let policy = CommandPolicy::new().deny_programs_also([" /usr/bin/Kubectl ", "", "two words"]);
        for command in [
            "kubectl delete pod x",
            "sudo /usr/local/bin/KUBECTL get pods",
            "uptime; kubectl",
        ] {
            assert!(denied(&policy.evaluate(command)), "{command}");
        }
        // As with the defaults, the arguments of a read-only program do not count.
        assert_eq!(policy.evaluate("grep kubectl notes.txt"), PolicyDecision::Allow);
        // The default denied programs stay.
        assert!(denied(&policy.evaluate("reboot")));
        // A name of several words matches no single word.
        assert_eq!(policy.evaluate("two words"), PolicyDecision::Allow);
    }

    #[test]
    fn extra_read_only_programs_make_the_words_after_them_data() {
        let command = "zstdcat reboot | head";
        assert!(denied(&CommandPolicy::new().evaluate(command)));
        let policy = CommandPolicy::new().read_only_programs_also(["ZSTDCAT"]);
        assert_eq!(policy.evaluate(command), PolicyDecision::Allow);
        // Only where that program runs: the word still counts in another program's command.
        assert!(denied(&policy.evaluate("zstdcat log | xargs reboot")));
    }

    #[test]
    fn power_programs_are_denied_wherever_a_command_starts() {
        let policy = CommandPolicy::new();
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
        let policy = CommandPolicy::new();
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

    #[test]
    fn only_bash_is_checked() {
        let policy = CommandPolicy::new();
        assert_eq!(
            policy.decision(&call("read_file", json!({"path": "/etc/hosts"}))),
            PolicyDecision::Allow
        );
        assert!(denied(&policy.decision(&call("bash", json!({})))));
    }
}
