//! The `ls` tool: the entries of one directory.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{SearchSettings, Wording, finish, first_lines, positive, quote, search_path};
use crate::agent::{Tool, ToolContext, ToolOutput};
use crate::ai::ToolDef;
use crate::tools::parse_args;

/// Entries shown by default (pi: ls's `DEFAULT_LIMIT`).
const LS_LIMIT: usize = 500;

/// Lists a directory.
#[derive(Default)]
pub struct LsTool {
    /// Where and how the listing runs.
    pub settings: SearchSettings,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LsArgs {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

impl LsArgs {
    fn parse(args: &Value) -> Result<Self, String> {
        let args: Self = parse_args(args)?;
        positive(args.limit, LS_LIMIT, "limit")?;
        Ok(args)
    }
}

#[async_trait]
impl Tool for LsTool {
    search_tool_basics!(crate::tools::LS);

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: crate::tools::LS.to_string(),
            description: format!(
                "List a directory: one entry per line, sorted, dotfiles included, directories ending in '/'. Lists the \
                 working directory unless path is given. Shows at most {LS_LIMIT} entries by default. Read-only."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Directory to list (default: working directory)" },
                    "limit": { "type": "integer", "minimum": 1, "description": format!("Most entries to show (default: {LS_LIMIT})") }
                },
                "additionalProperties": false
            }),
        }
    }

    fn validate_arguments(&self, args: &Value) -> Result<(), String> {
        LsArgs::parse(args).map(|_| ())
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, String> {
        let args = LsArgs::parse(&args)?;
        let limit = positive(args.limit, LS_LIMIT, "limit")?;
        let path = quote(&search_path(args.path.as_deref()));
        // -A: dotfiles without . and ..; -p: '/' after directories; -1: one per line.
        let script = format!("LC_ALL=C ls -1Ap -- {path} | {}", first_lines(limit));
        let searched = self.settings.run(&script, ctx).await?;
        finish(
            &searched,
            limit,
            Wording {
                empty: "(empty directory)",
                more: format!("more entries exist; showing the first {limit}. Raise limit to see more"),
                cut: None,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limit_and_the_arguments_are_checked() {
        assert!(LsArgs::parse(&json!({"limit": 0})).is_err());
        assert!(LsArgs::parse(&json!({"path": "x", "extra": 1})).is_err());
        assert!(LsArgs::parse(&json!({"path": "x", "limit": 3})).is_ok());
    }
}
