//! The `grep` tool: file contents matching a pattern, with ripgrep or POSIX grep.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    MAX_LINE_BYTES, SearchSettings, VCS_DIRECTORIES, Wording, choose_engine, finish, first_lines, positive, quote,
    ripgrep_excludes, search_path,
};
use crate::agent::{Tool, ToolContext, ToolOutput};
use crate::ai::ToolDef;
use crate::tools::parse_args;
use crate::tools::truncate::{MAX_BYTES, MAX_LINES};

/// Matches shown by default (pi: grep's `DEFAULT_LIMIT`).
const GREP_LIMIT: usize = 100;
/// The most context lines on each side.
const MAX_CONTEXT: usize = 10;

/// Searches file contents for a pattern.
#[derive(Default)]
pub struct GrepTool {
    /// Where and how the search runs.
    pub settings: SearchSettings,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    ignore_case: bool,
    #[serde(default)]
    literal: bool,
    #[serde(default)]
    context: usize,
    #[serde(default)]
    limit: Option<usize>,
}

impl GrepArgs {
    fn parse(args: &Value) -> Result<Self, String> {
        let args: Self = parse_args(args)?;
        if args.pattern.is_empty() {
            return Err("pattern is empty".into());
        }
        if args.context > MAX_CONTEXT {
            return Err(format!("context must be at most {MAX_CONTEXT}"));
        }
        positive(args.limit, GREP_LIMIT, "limit")?;
        Ok(args)
    }

    /// The shell script: ripgrep when installed, otherwise POSIX grep, each line cut, at most
    /// `lines` lines.
    fn script(&self, lines: usize) -> String {
        let path = quote(&search_path(self.path.as_deref()));
        let pattern = quote(&self.pattern);
        let mut rg =
            vec!["rg --no-config --line-number --with-filename --no-heading --color=never --hidden".to_string()];
        let mut grep = vec!["grep -rnHI".to_string()];
        if self.ignore_case {
            rg.push("--ignore-case".into());
            grep.push("-i".into());
        }
        if self.literal {
            rg.push("--fixed-strings".into());
            grep.push("-F".into());
        } else {
            grep.push("-E".into());
        }
        if let Some(glob) = self.glob.as_deref().filter(|glob| !glob.trim().is_empty()) {
            rg.push(format!("--glob {}", quote(glob)));
            // grep matches --include against file names only.
            let name = glob.rsplit('/').next().unwrap_or(glob);
            grep.push(format!("--include={}", quote(name)));
        }
        if self.context > 0 {
            rg.push(format!("--context {}", self.context));
            grep.push(format!("-C {}", self.context));
        }
        for directory in VCS_DIRECTORIES {
            grep.push(format!("--exclude-dir={directory}"));
        }
        rg.push(ripgrep_excludes());
        rg.push(format!("-- {pattern} {path}"));
        grep.push(format!("-e {pattern} -- {path}"));
        format!(
            "{} | {}",
            choose_engine(&rg.join(" "), "grep", &grep.join(" ")),
            first_lines(lines)
        )
    }
}

#[async_trait]
impl Tool for GrepTool {
    search_tool_basics!(crate::tools::GREP);

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: crate::tools::GREP.to_string(),
            description: format!(
                "Search file contents for a pattern; returns matching lines as path:line:text. Searches the working \
                 directory unless path names a directory or file. The pattern is a regular expression unless literal \
                 is true; respects .gitignore when ripgrep is installed. Shows at most {GREP_LIMIT} matches by default \
                 and {MAX_LINES} lines or {}KB; lines over {MAX_LINE_BYTES} bytes are cut. Read-only.",
                MAX_BYTES / 1024
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regular expression, or text when literal is true" },
                    "path": { "type": "string", "description": "Directory or file to search (default: working directory)" },
                    "glob": { "type": "string", "description": "Only files matching this glob, such as '*.rs' or 'src/**/*.ts'" },
                    "ignoreCase": { "type": "boolean", "description": "Match regardless of case (default: false)" },
                    "literal": { "type": "boolean", "description": "Treat the pattern as plain text (default: false)" },
                    "context": { "type": "integer", "minimum": 0, "maximum": MAX_CONTEXT, "description": "Lines to show before and after each match (default: 0)" },
                    "limit": { "type": "integer", "minimum": 1, "description": format!("Most matches to show (default: {GREP_LIMIT})") }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        }
    }

    fn validate_arguments(&self, args: &Value) -> Result<(), String> {
        GrepArgs::parse(args).map(|_| ())
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, String> {
        let args = GrepArgs::parse(&args)?;
        let limit = positive(args.limit, GREP_LIMIT, "limit")?;
        // A match with its context, and the separator between groups of them.
        let lines = limit.saturating_mul(2 * args.context + 2).min(MAX_LINES);
        let searched = self.settings.run(&args.script(lines), ctx).await?;
        // Without context every line is a match, so only `limit` of them are shown.
        let shown = if args.context == 0 { limit } else { lines };
        finish(
            &searched,
            shown,
            Wording {
                empty: "No matches found.",
                more: format!(
                    "more matches exist; showing about {limit}. Narrow the pattern, path or glob, or raise limit"
                ),
                cut: Some("long lines or output were cut"),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_control_is_excluded_after_the_models_glob() {
        let script = GrepArgs::parse(&json!({"pattern": "x", "glob": "*"}))
            .unwrap()
            .script(10);
        let model = script.find("--glob '*'").unwrap();
        let excluded = script.find("--glob '!.git'").unwrap();
        assert!(model < excluded, "{script}");
    }

    #[test]
    fn limits_and_context_are_checked() {
        assert!(GrepArgs::parse(&json!({"pattern": "x", "limit": 0})).is_err());
        assert!(GrepArgs::parse(&json!({"pattern": "x", "context": MAX_CONTEXT + 1})).is_err());
        assert!(GrepArgs::parse(&json!({"pattern": ""})).is_err());
        assert!(GrepArgs::parse(&json!({"pattern": "x", "ignoreCase": true, "context": 2})).is_ok());
    }
}
