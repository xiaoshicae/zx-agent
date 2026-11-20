//! The `ls` tool: the entries of one directory.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use zx_core::tools::{Effects, ExecutionMode, Tool, ToolContext, ToolOutput};

use super::{Wording, finish, first_lines, limit_or, quote, search, search_path};
use crate::ai::ToolDef;
use crate::executor::{Executor, LocalExecutor};
use crate::tools::parse_args;

/// Entries shown by default (pi: ls's `DEFAULT_LIMIT`).
const DEFAULT_LIMIT: usize = 500;

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
struct Args {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

impl Args {
    /// The arguments of a call, checked, and the most entries it shows.
    fn parse(args: &Value) -> Result<(Self, usize), String> {
        let args: Self = parse_args(args)?;
        let limit = limit_or(args.limit, DEFAULT_LIMIT)?;
        Ok((args, limit))
    }

    /// The shell script: the directory's entries, sorted, at most `limit + 1` of them.
    fn script(&self, limit: usize) -> String {
        let path = quote(&search_path(self.path.as_deref()));
        // -A: dotfiles without . and ..; -p: '/' after directories; -1: one per line.
        format!("LC_ALL=C ls -1Ap -- {path} | {}", first_lines(limit))
    }
}

#[async_trait]
impl Tool for LsTool {
    fn name(&self) -> &str {
        crate::tools::LS
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: crate::tools::LS.to_string(),
            description: format!(
                "List a directory: one entry per line, sorted, dotfiles included, directories ending in '/'. Lists the \
                 working directory unless path is given. Shows at most {DEFAULT_LIMIT} entries by default. Read-only."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Directory to list (default: working directory)" },
                    "limit": { "type": "integer", "minimum": 1, "description": format!("Most entries to show (default: {DEFAULT_LIMIT})") }
                },
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
                empty: "(empty directory)",
                more: "more entries exist",
                advice: "raise limit, or list a subdirectory",
                long_lines: "long names were truncated",
                also_not_applied: None,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limit_and_the_arguments_are_checked() {
        assert!(Args::parse(&json!({"limit": 0})).is_err());
        assert!(Args::parse(&json!({"path": "x", "extra": 1})).is_err());
        assert!(Args::parse(&json!({"path": "x", "limit": 3})).is_ok());
    }
}
