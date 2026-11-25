//! `write_file` tool: create or replace a whole file (pi: coding-agent/src/core/tools/write.ts).

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use zx_core::tools::{ExecutionMode, Tool, ToolContext, ToolOutput};

use super::edit_text::{add_patch, split_bom};
use super::mutation::{CHANGE_CANCELLED, FileMutationQueue, write_locked};
use super::{DEFAULT_MAX_FILE_BYTES, check_path, looks_binary, parse_args, path_text, resolve, unless_cancelled};
use crate::ai::ToolDef;
use crate::executor::{Executor, LocalExecutor};

/// Creates or overwrites files, creating missing parent directories. Replacing a text file,
/// its result details hold the patch of the change, as [`EDIT_FILE`](super::EDIT_FILE)'s do.
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

impl Args {
    /// The arguments of a call, checked.
    fn parse(args: &Value) -> Result<Self, String> {
        let args: Self = parse_args(args)?;
        check_path(&args.path)?;
        Ok(args)
    }
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
        Args::parse(args).map(|_| ())
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, String> {
        let args = Args::parse(&args)?;
        let shown = path_text(&args.path)?;
        let path = resolve(&ctx.cwd, &args.path)?;
        let turn = unless_cancelled(ctx, self.queue.turn(&path))
            .await
            .ok_or(CHANGE_CANCELLED)?;
        let replaced = unless_cancelled(ctx, self.replaced_text(&path))
            .await
            .ok_or(CHANGE_CANCELLED)?;
        let bytes = args.content.len();
        let mut details = Map::new();
        details.insert("path".into(), json!(path.display().to_string()));
        details.insert("bytes".into(), json!(bytes));
        if let Some(old) = &replaced {
            add_patch(&mut details, shown, split_bom(old).1, split_bom(&args.content).1);
        }
        write_locked(
            ctx,
            self.executor.clone(),
            path.clone(),
            args.content.into_bytes(),
            turn,
        )
        .await?;
        Ok(ToolOutput::ok(format!("Wrote {bytes} bytes to {shown}.")).with_details(Value::Object(details)))
    }
}

impl WriteFileTool {
    /// The text of the file at `path` that a write replaces, for the patch of the change, or
    /// `None` when there is none to show: the file is new, or it is binary, not UTF-8 or past
    /// [`DEFAULT_MAX_FILE_BYTES`], or it cannot be read. None of these stops the write.
    async fn replaced_text(&self, path: &Path) -> Option<String> {
        // A failure only means there is no patch to show; the write goes on either way.
        let bytes = self.executor.read_file(path, DEFAULT_MAX_FILE_BYTES).await.ok()?;
        if looks_binary(&bytes) {
            return None;
        }
        String::from_utf8(bytes).ok()
    }
}
