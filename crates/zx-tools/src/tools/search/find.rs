//! The `find` tool: file paths matching a glob, with `rg --files` or POSIX find.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    SearchSettings, VCS_DIRECTORIES, Wording, choose_engine, finish, first_lines, positive, quote, ripgrep_excludes,
    search_path,
};
use crate::agent::{Tool, ToolContext, ToolOutput};
use crate::ai::ToolDef;
use crate::tools::parse_args;

/// Paths shown by default (pi: find's `DEFAULT_LIMIT`).
const FIND_LIMIT: usize = 1000;

/// Finds files whose paths match a glob.
#[derive(Default)]
pub struct FindTool {
    /// Where and how the search runs.
    pub settings: SearchSettings,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FindArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

impl FindArgs {
    fn parse(args: &Value) -> Result<Self, String> {
        let args: Self = parse_args(args)?;
        if args.pattern.trim().is_empty() {
            return Err("pattern is empty".into());
        }
        positive(args.limit, FIND_LIMIT, "limit")?;
        Ok(args)
    }

    /// The shell script, run in the search directory: `rg --files` when installed, otherwise
    /// `find`, sorted, at most `limit + 1` paths.
    fn script(&self, limit: usize) -> String {
        let directory = quote(&search_path(self.path.as_deref()));
        let pattern = self.pattern.trim();
        // find matches a pattern without a slash against the name; one with a slash against
        // the path from the search directory, where `*` also matches `/`, so `**/` can go.
        let test = if pattern.contains('/') {
            let path = pattern.trim_start_matches("./").replace("**/", "");
            format!("-path {}", quote(&format!("./{path}")))
        } else {
            format!("-name {}", quote(pattern))
        };
        let rg = format!(
            "rg --no-config --files --hidden --color=never --glob {} {}",
            quote(pattern),
            ripgrep_excludes()
        );
        let prune = VCS_DIRECTORIES
            .iter()
            .map(|directory| format!("-name {directory}"))
            .collect::<Vec<_>>()
            .join(" -o ");
        let find = format!("find . \\( {prune} \\) -prune -o {test} -type f -print | sed 's|^\\./||'");
        format!(
            "cd -- {directory} || exit 2; {} | LC_ALL=C sort | {}",
            choose_engine(&rg, "find", &find),
            first_lines(limit)
        )
    }
}

#[async_trait]
impl Tool for FindTool {
    search_tool_basics!(crate::tools::FIND);

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: crate::tools::FIND.to_string(),
            description: format!(
                "Find files by glob, such as '*.rs', '**/*.json' or 'src/**/*.spec.ts'; returns paths relative to the \
                 searched directory, sorted. A pattern without '/' matches file names at any depth. Respects .gitignore \
                 when ripgrep is installed. Shows at most {FIND_LIMIT} paths by default. Read-only."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob for the file paths" },
                    "path": { "type": "string", "description": "Directory to search (default: working directory)" },
                    "limit": { "type": "integer", "minimum": 1, "description": format!("Most paths to show (default: {FIND_LIMIT})") }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        }
    }

    fn validate_arguments(&self, args: &Value) -> Result<(), String> {
        FindArgs::parse(args).map(|_| ())
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, String> {
        let args = FindArgs::parse(&args)?;
        let limit = positive(args.limit, FIND_LIMIT, "limit")?;
        let searched = self.settings.run(&args.script(limit), ctx).await?;
        finish(
            &searched,
            limit,
            Wording {
                empty: "No files found.",
                more: format!(
                    "more files match; showing the first {limit}. Narrow the pattern or path, or raise limit"
                ),
                cut: Some("long paths or output were cut"),
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
            FindArgs {
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
    fn version_control_is_excluded_after_the_models_glob() {
        let script = FindArgs::parse(&json!({"pattern": "*"})).unwrap().script(10);
        let model = script.find("--glob '*'").unwrap();
        let excluded = script.find("--glob '!.git'").unwrap();
        assert!(model < excluded, "{script}");
    }

    #[test]
    fn a_blank_pattern_is_refused() {
        assert!(FindArgs::parse(&json!({"pattern": " "})).is_err());
        assert!(FindArgs::parse(&json!({"pattern": "*.rs", "limit": 0})).is_err());
    }
}
