//! `write_file` tool: create or replace a whole file (pi: coding-agent/src/core/tools/write.ts).

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::mutation::FileMutationQueue;
use super::{check_path, parse_args, resolve, unless_cancelled};
use crate::agent::{ExecutionMode, Tool, ToolContext, ToolOutput};
use crate::ai::ToolDef;
use crate::executor::{Executor, LocalExecutor};

/// Creates or overwrites files, creating missing parent directories.
pub struct WriteFileTool {
    /// Where files are written. Defaults to this machine.
    pub executor: Arc<dyn Executor>,
    /// Shared with `edit_file`, so that changes to one file happen one at a time.
    pub queue: Arc<FileMutationQueue>,
}

impl Default for WriteFileTool {
    fn default() -> Self {
        Self {
            executor: Arc::new(LocalExecutor::default()),
            queue: Arc::default(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        super::WRITE_FILE
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: super::WRITE_FILE.to_string(),
            description:
                "Write content to a file. Creates the file if it does not exist and overwrites it if it does; \
                          creates missing parent directories. Use it for new files or complete rewrites, and edit_file \
                          for changes to part of a file. Relative paths resolve against the working directory."
                    .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path" },
                    "content": { "type": "string", "description": "The whole new content of the file" }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        }
    }

    fn execution_mode(&self) -> ExecutionMode {
        self.executor.execution_mode()
    }

    fn validate_arguments(&self, args: &Value) -> Result<(), String> {
        let args: Args = parse_args(args)?;
        check_path(&args.path)
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, String> {
        let args: Args = parse_args(&args)?;
        let path = resolve(&ctx.cwd, &args.path)?;
        let cancelled = || "write cancelled before it started".to_string();
        let _turn = unless_cancelled(ctx, self.queue.turn(&path))
            .await
            .ok_or_else(cancelled)?;
        // Checked last before writing: a write is not interrupted once it has started.
        if ctx.cancel.is_cancelled() {
            return Err(cancelled());
        }
        self.executor.write_file(&path, args.content.as_bytes()).await?;
        let bytes = args.content.len();
        Ok(ToolOutput::ok(format!("Wrote {bytes} bytes to {}.", args.path.trim()))
            .with_details(json!({ "path": path.display().to_string(), "bytes": bytes })))
    }
}
