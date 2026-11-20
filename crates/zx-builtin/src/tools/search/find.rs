//! The `find` tool: file paths matching a glob, with `rg --files` or POSIX find.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    VCS_DIRECTORIES, WITHOUT_DOT_SLASH, Wording, choose_engine, finish, first_lines, glob_of, limit_or, quote,
    ripgrep_excludes, search, search_path,
};
use crate::ai::ToolDef;
use crate::executor::{Executor, LocalExecutor};
use crate::tools::parse_args;
use zx_core::tools::{Effects, ExecutionMode, Tool, ToolContext, ToolOutput};

/// Paths shown by default (pi: find's `DEFAULT_LIMIT`).
const DEFAULT_LIMIT: usize = 1000;
/// In a glob, any number of directories, none included.
const ANY_DEPTH: &str = "**/";
/// The most [`ANY_DEPTH`] in one glob: each doubles the patterns find is given.
const MAX_ANY_DEPTH: usize = 4;

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
        let glob = glob_of(&args.pattern);
        if glob.is_empty() {
            return Err("pattern is empty".into());
        }
        if glob.matches(ANY_DEPTH).count() > MAX_ANY_DEPTH {
            return Err(format!(
                "pattern has more than {MAX_ANY_DEPTH} `{ANY_DEPTH}`; write one with fewer"
            ));
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
    // find matches a pattern without a slash against the name, one with a slash against the
    // path from the search directory.
    let test = if glob.contains('/') {
        let paths: Vec<String> = find_paths(glob)
            .iter()
            .map(|path| format!("-path {}", quote(&format!("./{path}"))))
            .collect();
        format!("\\( {} \\)", paths.join(" -o "))
    } else {
        format!("-name {}", quote(glob))
    };
    let prune = VCS_DIRECTORIES
        .iter()
        .map(|directory| format!("-name {directory}"))
        .collect::<Vec<_>>()
        .join(" -o ");
    format!("find . \\( {prune} \\) -prune -o {test} -type f -print | {WITHOUT_DOT_SLASH}")
}

/// The patterns that find's `-path` takes for `glob`, which together match what it matches.
/// In find `*` also matches `/`, so each [`ANY_DEPTH`] is either nothing or `*/`, one
/// directory or more: twice as many patterns for each, which [`MAX_ANY_DEPTH`] bounds.
fn find_paths(glob: &str) -> Vec<String> {
    match glob.split_once(ANY_DEPTH) {
        None => vec![glob.to_string()],
        Some((before, after)) => find_paths(after)
            .into_iter()
            .flat_map(|rest| [format!("{before}{rest}"), format!("{before}*/{rest}")])
            .collect(),
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
                advice: "Narrow the pattern or path, or raise limit",
                long_lines: "long paths were cut",
                also_not_applied: None,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_turns_a_glob_with_a_slash_into_a_path_test() {
        let script = |pattern: &str| {
            Args {
                pattern: pattern.into(),
                path: None,
                limit: None,
            }
            .script(10)
        };
        assert!(script("*.rs").contains("-name '*.rs'"));
        assert!(script("src/**/*.ts").contains("-path './src/*.ts'"));
        assert!(script("**/*.json").contains("-path './*.json'"));
        assert!(script("./a/b.txt").contains("-path './a/b.txt'"));
    }

    #[test]
    fn a_glob_that_starts_at_the_search_directory_reaches_ripgrep_without_the_dot() {
        let script = Args::parse(&json!({"pattern": "./a/*.txt"})).unwrap().0.script(10);
        assert!(script.contains("--glob 'a/*.txt'"), "{script}");
    }

    #[test]
    fn a_glob_names_any_depth_a_few_times_at_most() {
        let pattern = |times: usize| json!({"pattern": format!("{}x", "**/".repeat(times))});
        assert!(Args::parse(&pattern(MAX_ANY_DEPTH)).is_ok());
        let error = Args::parse(&pattern(MAX_ANY_DEPTH + 1)).err().unwrap();
        assert_eq!(error, "pattern has more than 4 `**/`; write one with fewer");
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
