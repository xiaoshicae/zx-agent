//! The `ls` tool: the entries of one directory.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Wording, finish, first_lines, positive, quote, search, search_path};
use crate::ai::ToolDef;
use crate::executor::{Executor, LocalExecutor};
use crate::tools::parse_args;
use zx_core::tools::{Tool, ToolContext, ToolOutput};

/// Entries shown by default (pi: ls's `DEFAULT_LIMIT`).
const LS_LIMIT: usize = 500;

/// Lists a directory.
pub struct LsTool {
    /// Where the listing runs, and so what it must not see: on this machine, the variables in
    /// [`LocalExecutor::hidden_env`]. Defaults to this machine, hiding nothing.
    pub executor: Arc<dyn Executor>,
}

impl Default for LsTool {
    fn default() -> Self {
        Self {
            executor: Arc::new(LocalExecutor::default()),
        }
    }
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
        let searched = search(self.executor.as_ref(), &script, ctx).await?;
        finish(
            &searched,
            limit,
            Wording {
                empty: "(empty directory)",
                more: "more entries exist",
                advice: "Raise limit, or list a subdirectory",
                long_lines: "long names were cut",
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
