//! Bash commands that run without asking where a rule would otherwise ask.

use serde::Deserialize;

use super::{begins_with, normalize, normalize_all};

/// Punctuation that a command approved without asking may contain, besides letters, digits
/// and whitespace. None of it lets the shell run or read anything but what the words spell;
/// every other character asks, such as quotes and `\` (which hide words from these checks),
/// `$` (expansion), `*`, `?` and `[` (file names), `{` (brace expansion), parentheses, `#`,
/// and the syntax the allowlist rejects.
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
/// - it is a `|` pipeline whose every segment starts with a listed prefix, compared like
///   the allowlist (whitespace collapsed, lowercased);
/// - it consists of letters, digits, whitespace and the punctuation `- _ . / : = , @ + | ^ ~ !`,
///   so no quoting, escaping, expansion or other shell syntax can change what runs;
/// - it contains no URL (`://`) and none of the [`except`](Self::except) texts;
/// - a segment that runs a program given [`options`](Self::options) passes only those options.
///
/// Like the allowlist the check is lexical: it decides only whether to ask, and never turns
/// a denial into an approval. It knows no particular program: which of a program's options
/// are safe to pass without asking is configuration.
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
