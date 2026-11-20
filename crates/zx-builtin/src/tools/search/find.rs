//! The `find` tool: file paths matching a glob, with `rg --files` or POSIX find.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use zx_core::tools::{Effects, ExecutionMode, Tool, ToolContext, ToolOutput};

use super::{
    VCS_DIRECTORIES, WITHOUT_DOT_SLASH, Wording, choose_engine, finish, first_lines, glob_of, limit_or, quote,
    ripgrep_excludes, search, search_path,
};
use crate::ai::ToolDef;
use crate::executor::{Executor, LocalExecutor};
use crate::tools::parse_args;

/// Paths shown by default (pi: find's `DEFAULT_LIMIT`).
const DEFAULT_LIMIT: usize = 1000;
/// In a glob, any number of directories, none included.
const ANY_DEPTH: &str = "**/";
/// At the end of a glob, everything below the directory before it.
const EVERYTHING_BELOW: &str = "**";

/// Finds files whose paths match a glob.
pub struct FindTool {
    /// Where the search runs, and so what it must not see: on this machine, the variables in
    /// [`LocalExecutor::hidden_env`]. Defaults to this machine, hiding nothing.
    pub executor: Arc<dyn Executor>,
}

impl Default for FindTool {
    fn default() -> Self {
        Self {
            executor: Arc::new(LocalExecutor::default()),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

impl Args {
    /// The arguments of a call, checked, and the most paths it shows.
    fn parse(args: &Value) -> Result<(Self, usize), String> {
        let args: Self = parse_args(args)?;
        if glob_of(&args.pattern).is_empty() {
            return Err("pattern is empty".into());
        }
        let limit = limit_or(args.limit, DEFAULT_LIMIT)?;
        Ok((args, limit))
    }

    /// The shell script, run in the search directory: `rg --files` when installed, otherwise
    /// `find`, sorted, at most `limit + 1` paths.
    fn script(&self, limit: usize) -> String {
        let directory = quote(&search_path(self.path.as_deref()));
        let glob = glob_of(&self.pattern);
        format!(
            "cd -- {directory} || exit 2; {} | LC_ALL=C sort | {}",
            choose_engine(&ripgrep(glob), "find", &posix_find(glob)),
            first_lines(limit)
        )
    }
}

/// `rg --files` for the paths `glob` matches.
fn ripgrep(glob: &str) -> String {
    format!(
        "rg --no-config --files --hidden --color=never --glob {} {}",
        quote(glob),
        ripgrep_excludes()
    )
}

/// POSIX find for the paths `glob` matches, printed as ripgrep prints them, without `./`.
fn posix_find(glob: &str) -> String {
    // find matches a pattern without a slash against the name. Its `-path` would let `*` match
    // `/` too, so a pattern with one is matched against each path afterwards, by bytes as sed
    // reads them.
    let (test, filter) = if glob.contains('/') {
        (
            String::new(),
            format!(" | LC_ALL=C grep -E {}", quote(&path_regex(glob))),
        )
    } else {
        (format!("-name {} ", quote(glob)), String::new())
    };
    let prune = VCS_DIRECTORIES
        .iter()
        .map(|directory| format!("-name {directory}"))
        .collect::<Vec<_>>()
        .join(" -o ");
    format!("find . \\( {prune} \\) -prune -o {test}-type f -print | {WITHOUT_DOT_SLASH}{filter}")
}

/// The extended regular expression for the paths from the search directory that `glob`
/// matches as ripgrep matches it: `*` and `?` stay within one name, a bracket expression is
/// one character of the set (`[!…]` of the others), [`ANY_DEPTH`] is any number of
/// directories and a final [`EVERYTHING_BELOW`] everything below, and `\` makes the next
/// character plain.
fn path_regex(glob: &str) -> String {
    let mut regex = String::from("^");
    let mut rest = glob;
    // Whether `rest` starts a name: at the start of the glob and after a `/`.
    let mut name_starts = true;
    while let Some(next) = rest.chars().next() {
        let (piece, taken) = if name_starts && rest.starts_with(ANY_DEPTH) {
            ("(.*/)?".to_string(), ANY_DEPTH.len())
        } else if name_starts && rest == EVERYTHING_BELOW {
            (".*".to_string(), EVERYTHING_BELOW.len())
        } else {
            match next {
                '*' => ("[^/]*".to_string(), 1),
                '?' => ("[^/]".to_string(), 1),
                '[' => bracket(rest).unwrap_or_else(|| (plain('['), 1)),
                '\\' => rest[1..]
                    .chars()
                    .next()
                    .map_or((plain('\\'), 1), |escaped| (plain(escaped), 1 + escaped.len_utf8())),
                other => (plain(other), other.len_utf8()),
            }
        };
        name_starts = rest[..taken].ends_with('/');
        regex.push_str(&piece);
        rest = &rest[taken..];
    }
    regex.push('$');
    regex
}

/// The bracket expression that `glob` starts with, as a regular expression, and its length;
/// `None` when it is not closed. A `]` right after the opening is one of the set.
fn bracket(glob: &str) -> Option<(String, usize)> {
    let negated = glob[1..].starts_with('!');
    let set_starts = if negated { 2 } else { 1 };
    let first = glob[set_starts..].chars().next()?.len_utf8();
    let set_len = first + glob[set_starts + first..].find(']')?;
    let set = &glob[set_starts..set_starts + set_len];
    let not = if negated { "^" } else { "" };
    Some((format!("[{not}{set}]"), set_starts + set_len + 1))
}

/// `character` as a regular expression that matches only it.
fn plain(character: char) -> String {
    if "^.[$()|*+?{\\".contains(character) {
        format!("\\{character}")
    } else {
        character.to_string()
    }
}

#[async_trait]
impl Tool for FindTool {
    fn name(&self) -> &str {
        crate::tools::FIND
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: crate::tools::FIND.to_string(),
            description: format!(
                "Find files by glob, such as '*.rs', '**/*.json' or 'src/**/*.spec.ts'; returns paths relative to the \
                 searched directory, sorted. A pattern without '/' matches file names at any depth. Respects .gitignore \
                 when ripgrep is installed. Shows at most {DEFAULT_LIMIT} paths by default. Read-only."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob for the file paths" },
                    "path": { "type": "string", "description": "Directory to search (default: working directory)" },
                    "limit": { "type": "integer", "minimum": 1, "description": format!("Most paths to show (default: {DEFAULT_LIMIT})") }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        }
    }

    fn execution_mode(&self) -> ExecutionMode {
        self.executor.execution_mode()
    }

    fn effects(&self) -> Effects {
        Effects::ReadOnly
    }

    fn validate_arguments(&self, args: &Value) -> Result<(), String> {
        Args::parse(args).map(|_| ())
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, String> {
        let (args, limit) = Args::parse(&args)?;
        let searched = search(self.executor.as_ref(), &args.script(limit), ctx).await?;
        finish(
            &searched,
            limit,
            Wording {
                empty: "No files found.",
                more: "more files match",
                advice: "narrow the pattern or path, or raise limit",
                long_lines: "long paths were truncated",
                also_not_applied: None,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_matches_a_glob_with_a_slash_against_the_whole_path() {
        let script = |pattern: &str| Args::parse(&json!({"pattern": pattern})).unwrap().0.script(10);
        assert!(script("*.rs").contains("-name '*.rs'"));
        assert!(script("src/**/*.ts").contains(r"grep -E '^src/(.*/)?[^/]*\.ts$'"));
    }

    #[test]
    fn a_glob_that_starts_at_the_search_directory_reaches_both_programs_without_the_dot() {
        let script = Args::parse(&json!({"pattern": "./a/*.txt"})).unwrap().0.script(10);
        assert!(script.contains("--glob 'a/*.txt'"), "{script}");
        assert!(script.contains(r"grep -E '^a/[^/]*\.txt$'"), "{script}");
    }

    #[test]
    fn the_regex_of_a_glob_takes_its_other_characters_as_they_are() {
        assert_eq!(path_regex(r"a.b/c+(1)/\*[x"), r"^a\.b/c\+\(1\)/\*\[x$");
    }

    /// The paths POSIX find prints for `glob` in a directory that holds `files`, sorted.
    #[cfg(unix)]
    fn found_by_find(files: &[&str], glob: &str) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        for file in files {
            let path = dir.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        let output = std::process::Command::new("sh")
            .args(["-c", &format!("{} | LC_ALL=C sort", posix_find(glob))])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(output.stderr.is_empty(), "{output:?}");
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(String::from)
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn find_matches_a_name_under_any_number_of_directories() {
        let files = ["b.txt", "a/b.txt", "a/x/b.txt", "a/x/c.txt", "z/b.txt"];
        assert_eq!(
            found_by_find(&files, "**/b.txt"),
            ["a/b.txt", "a/x/b.txt", "b.txt", "z/b.txt"]
        );
        assert_eq!(found_by_find(&files, "a/**/b.txt"), ["a/b.txt", "a/x/b.txt"]);
        assert_eq!(found_by_find(&files, "a/**/x/**/*.txt"), ["a/x/b.txt", "a/x/c.txt"]);
        assert_eq!(found_by_find(&files, "**/**/c.txt"), ["a/x/c.txt"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_star_in_a_path_stays_within_one_name_as_with_ripgrep() {
        let files = ["src/a.ts", "src/x/b.ts", "pkg/src/c.ts"];
        assert_eq!(found_by_find(&files, "src/*.ts"), ["src/a.ts"]);
        assert_eq!(found_by_find(&files, "src/?.ts"), ["src/a.ts"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_final_any_depth_takes_everything_below() {
        let files = ["src/a.ts", "src/x/b.ts", "top.ts"];
        assert_eq!(found_by_find(&files, "src/**"), ["src/a.ts", "src/x/b.ts"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_bracket_expression_is_one_character_of_its_set_or_of_the_others() {
        let files = ["d/a1.txt", "d/b1.txt", "d/x1.txt"];
        assert_eq!(found_by_find(&files, "d/[ab]1.txt"), ["d/a1.txt", "d/b1.txt"]);
        assert_eq!(found_by_find(&files, "d/[!ab]1.txt"), ["d/x1.txt"]);
    }

    #[test]
    fn version_control_is_excluded_after_the_models_glob() {
        let script = Args::parse(&json!({"pattern": "*"})).unwrap().0.script(10);
        let model = script.find("--glob '*'").unwrap();
        let excluded = script.find("--glob '!.git'").unwrap();
        assert!(model < excluded, "{script}");
    }

    #[test]
    fn a_blank_pattern_is_refused() {
        assert!(Args::parse(&json!({"pattern": " "})).is_err());
        assert!(Args::parse(&json!({"pattern": "./"})).is_err());
        assert!(Args::parse(&json!({"pattern": "*.rs", "limit": 0})).is_err());
    }
}
