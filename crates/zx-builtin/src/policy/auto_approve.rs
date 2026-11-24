//! Bash commands that run without asking where a rule would otherwise ask, and why the others
//! ask.

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;

use super::{begins_with, collapse_whitespace, non_empty, normalize};
use crate::text::clip;

/// The most characters of a pipeline segment that [`Uncovered::Unlisted`] quotes: enough to
/// tell which segment it is, while the approval shows the whole command.
const MAX_QUOTED_SEGMENT_CHARS: usize = 60;

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
    /// Check that every option can match a command: long names without leading dashes, `=` or
    /// whitespace, single letters or digits for the short options, and no letter that is both
    /// a flag and an option with a value. An option that breaks these rules never matches, so
    /// it would only make more commands ask. The error names the first such option.
    pub fn validate(&self) -> Result<(), String> {
        for name in &self.long {
            if name.is_empty() || name.starts_with('-') || name.contains(|c: char| c.is_whitespace() || c == '=') {
                return Err(format!(
                    "long option {name:?} must be a name without leading dashes, `=` or whitespace, such as \"namespace\""
                ));
            }
        }
        for letter in self.short_with_value.iter().chain(&self.short_flags) {
            if !letter.is_ascii_alphanumeric() {
                return Err(format!("short option {letter:?} must be a letter or digit"));
            }
        }
        match self
            .short_flags
            .iter()
            .find(|letter| self.short_with_value.contains(letter))
        {
            Some(letter) => Err(format!("{letter:?} cannot be both a flag and an option with a value")),
            None => Ok(()),
        }
    }

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

/// What of a bash command an [`AutoApprove`] list does not cover, so that the command asks:
/// the first thing found, in the order of the conditions listed there. Its text tells the
/// person who approves why the command was not auto-approved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Uncovered {
    /// A character that is not a letter, a digit, whitespace or the plain punctuation, such
    /// as a quote, `$` or `>`.
    Syntax(char),
    /// A URL (`://`).
    Url,
    /// One of the [`except`](AutoApprove::except) texts, as they are compared.
    Excepted(String),
    /// A pipeline segment, its whitespace collapsed, that starts with none of the prefixes.
    Unlisted(String),
    /// An option that a segment passes and the [`options`](AutoApprove::options) of its
    /// program, named as they are looked up, do not list.
    Option { program: String, option: String },
}

impl fmt::Display for Uncovered {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax(character) => write!(
                formatter,
                "it contains {character:?}, which can change what the shell runs"
            ),
            Self::Url => write!(formatter, "it contains a URL"),
            Self::Excepted(text) => write!(formatter, "it contains {text:?}, which keeps commands asking"),
            Self::Unlisted(segment) => write!(
                formatter,
                "{:?} starts with none of the auto-approved commands",
                clip(segment, MAX_QUOTED_SEGMENT_CHARS)
            ),
            Self::Option { program, option } => {
                write!(formatter, "{option} is not among the options listed for {program}")
            }
        }
    }
}

/// Bash commands that run without asking where a rule would otherwise ask for approval.
///
/// A command is covered only when all of these hold:
/// - it is a `|` pipeline whose every segment starts with a listed prefix, compared with
///   whitespace collapsed and in the case it is written: where file names keep their case,
///   `HEAD` is another program than `head`;
/// - it consists of letters, digits, whitespace and the punctuation `- _ . / : = , @ + | ^ ~ !`,
///   so no quoting, escaping, expansion or other shell syntax can change what runs;
/// - it contains no URL (`://`) and none of the [`except`](Self::except) texts;
/// - a segment that runs a program given [`options`](Self::options) passes only those options.
///
/// The check is lexical: it decides only whether to ask, and never turns a denial into an
/// approval. It knows no particular program: which of a program's options are safe to pass
/// without asking is configuration. [`uncovered`](Self::uncovered) says which condition a
/// command fails.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AutoApprove {
    /// The prefixes with their whitespace collapsed, in the case they were given.
    prefixes: Vec<String>,
    /// Normalized texts that keep a command asking.
    except: Vec<String>,
    /// Lowercased program names and the options their segments may pass.
    options: BTreeMap<String, ProgramOptions>,
}

impl AutoApprove {
    /// Commands starting with one of `prefixes`, such as `grep`, in the same case. An empty
    /// list covers no command.
    pub fn new(prefixes: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        Self {
            prefixes: non_empty(prefixes, collapse_whitespace),
            except: Vec::new(),
            options: BTreeMap::new(),
        }
    }

    /// Texts that keep a command asking wherever they appear in it, compared with whitespace
    /// collapsed and without regard to case, such as `secret` (which also matches `secrets`
    /// and `SECRET`).
    pub fn except(mut self, texts: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        self.except.extend(non_empty(texts, normalize));
        self
    }

    /// Limit the segments that run `program`, their first word compared without case, to
    /// `options`: any other option asks. Segments of programs given no options may pass any.
    /// A second call for the same program replaces the first. Check `options` with
    /// [`ProgramOptions::validate`] first: an option it refuses never matches.
    pub fn options(mut self, program: &str, options: ProgramOptions) -> Self {
        self.options.insert(normalize(program), options);
        self
    }

    /// Whether no prefix is listed, so that every command asks.
    pub(super) fn is_empty(&self) -> bool {
        self.prefixes.is_empty()
    }

    /// Whether `command` may run without asking: [`uncovered`](Self::uncovered) finds nothing.
    pub fn covers(&self, command: &str) -> bool {
        self.uncovered(command).is_none()
    }

    /// What of `command` this list does not cover, so that it asks; `None` when it may run
    /// without asking. An empty list covers no command.
    pub fn uncovered(&self, command: &str) -> Option<Uncovered> {
        if let Some(character) = command
            .chars()
            .find(|&c| !(c.is_alphanumeric() || c == ' ' || c == '\t' || PLAIN_PUNCTUATION.contains(&c)))
        {
            return Some(Uncovered::Syntax(character));
        }
        let normalized = normalize(command);
        if normalized.contains("://") {
            return Some(Uncovered::Url);
        }
        if let Some(text) = self.except.iter().find(|text| normalized.contains(text.as_str())) {
            return Some(Uncovered::Excepted(text.clone()));
        }
        // Without quotes in the command, every `|` separates two segments.
        command.split('|').find_map(|raw| {
            let segment = collapse_whitespace(raw);
            if self.prefixes.iter().any(|prefix| begins_with(&segment, prefix)) {
                self.unlisted_option(&segment)
            } else {
                Some(Uncovered::Unlisted(segment))
            }
        })
    }

    /// The first option that a pipeline segment, its whitespace collapsed, passes and its
    /// program may not. The program is looked up without regard to case, unlike the prefixes:
    /// that only puts more segments under a list of options.
    fn unlisted_option(&self, segment: &str) -> Option<Uncovered> {
        let mut words = segment.split(' ');
        let program = words.next()?.to_lowercase();
        let options = self.options.get(&program)?;
        let option = words
            .filter(|word| word.starts_with('-'))
            .find(|option| !options.allows(option))?;
        Some(Uncovered::Option {
            program,
            option: option.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_uncovered_command_says_what_keeps_it_asking() {
        let list = AutoApprove::new(["kubectl get", "grep"])
            .except(["Secret"])
            .options("kubectl", kubectl_read_only());
        for (command, uncovered) in [
            ("grep x app.log > copy", Uncovered::Syntax('>')),
            ("grep \"a b\" app.log", Uncovered::Syntax('"')),
            ("grep a\nrm b", Uncovered::Syntax('\n')),
            ("kubectl get -f https://example.com/pods.yaml", Uncovered::Url),
            ("kubectl get SECRETS -A", Uncovered::Excepted("secret".into())),
            ("kubectl get pods |  rm  -rf x", Uncovered::Unlisted("rm -rf x".into())),
            (
                "kubectl get pods -n prod -w",
                Uncovered::Option {
                    program: "kubectl".into(),
                    option: "-w".into(),
                },
            ),
        ] {
            assert_eq!(list.uncovered(command), Some(uncovered), "{command}");
        }
        assert_eq!(list.uncovered("kubectl get pods -n prod | grep web"), None);
        // An empty list covers nothing, and says which segment it found unlisted.
        assert_eq!(
            AutoApprove::default().uncovered("ls"),
            Some(Uncovered::Unlisted("ls".into()))
        );
    }

    #[test]
    fn why_a_command_asks_is_told_in_words_for_the_approver() {
        for (uncovered, text) in [
            (
                Uncovered::Syntax('$'),
                "it contains '$', which can change what the shell runs",
            ),
            (Uncovered::Url, "it contains a URL"),
            (
                Uncovered::Excepted("secret".into()),
                "it contains \"secret\", which keeps commands asking",
            ),
            (
                Uncovered::Unlisted("rm -rf x".into()),
                "\"rm -rf x\" starts with none of the auto-approved commands",
            ),
            (
                Uncovered::Option {
                    program: "kubectl".into(),
                    option: "--raw".into(),
                },
                "--raw is not among the options listed for kubectl",
            ),
        ] {
            assert_eq!(uncovered.to_string(), text);
        }
        // A long segment is quoted only as far as it tells which one it is.
        let long = Uncovered::Unlisted("x".repeat(MAX_QUOTED_SEGMENT_CHARS + 1)).to_string();
        assert!(
            long.starts_with(&format!("\"{}...\"", "x".repeat(MAX_QUOTED_SEGMENT_CHARS))),
            "{long}"
        );
    }

    #[test]
    fn program_options_that_cannot_match_a_command_are_invalid() {
        for (options, expected) in [
            (
                ProgramOptions {
                    long: vec!["--namespace".into()],
                    ..ProgramOptions::default()
                },
                "without leading dashes",
            ),
            (
                ProgramOptions {
                    long: vec!["output=json".into()],
                    ..ProgramOptions::default()
                },
                "`=` or whitespace",
            ),
            (
                ProgramOptions {
                    short_flags: vec!['-'],
                    ..ProgramOptions::default()
                },
                "letter or digit",
            ),
            (
                ProgramOptions {
                    short_flags: vec!['n'],
                    short_with_value: vec!['n'],
                    ..ProgramOptions::default()
                },
                "both a flag and an option with a value",
            ),
        ] {
            let error = options.validate().unwrap_err();
            assert!(error.contains(expected), "{options:?}: {error}");
        }
        assert_eq!(kubectl_read_only().validate(), Ok(()));
    }

    #[test]
    fn auto_approval_covers_listed_read_only_pipelines() {
        let list = AutoApprove::new(["kubectl get", "grep", "jq"]);
        for command in [
            "kubectl get pods -A | grep -v Running",
            "kubectl  get\tpods -n prod",
            "kubectl get pods -o json | jq -s .",
            "grep -s error app.log",
        ] {
            assert!(list.covers(command), "{command}");
        }
    }

    #[test]
    fn auto_approval_tells_programs_apart_by_case() {
        // Where file names keep their case, `HEAD` and `head` are two programs.
        let list = AutoApprove::new(["head", "kubectl get"]);
        for command in [
            "HEAD x",
            "Head -n 1 app.log",
            "KUBECTL  get\tpods -n prod",
            "kubectl GET pods",
            "kubectl get pods | HEAD",
        ] {
            assert!(!list.covers(command), "{command}");
        }
        assert!(list.covers("head -n 1 app.log"));
        let upper = AutoApprove::new(["HEAD"]);
        assert!(upper.covers("HEAD x"));
        assert!(!upper.covers("head x"));
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
    fn a_programs_options_limit_it_however_its_name_is_cased() {
        // The prefix lets `Kubectl` run; the options it may pass are those listed for kubectl.
        let list = AutoApprove::new(["Kubectl get"]).options("kubectl", kubectl_read_only());
        assert!(list.covers("Kubectl get pods -n prod"));
        assert!(!list.covers("Kubectl get pods --raw /api"));
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
