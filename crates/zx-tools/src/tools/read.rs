//! `read_file` tool: read a text file in pages.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::truncate::{MAX_BYTES, MAX_LINES, truncate_head};
use super::{DEFAULT_MAX_FILE_BYTES, check_path, looks_binary, parse_args, resolve, unless_cancelled};
use crate::agent::{Effects, ExecutionMode, Tool, ToolContext, ToolOutput};
use crate::ai::ToolDef;
use crate::executor::{Executor, LocalExecutor};

/// Reads text files in pages of bounded size.
pub struct ReadFileTool {
    /// Larger files are refused; the model should use `bash` with head, tail, or grep.
    pub max_file_bytes: u64,
    /// Where files are read. Defaults to this machine.
    pub executor: Arc<dyn Executor>,
}

impl Default for ReadFileTool {
    fn default() -> Self {
        Self {
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            executor: Arc::new(LocalExecutor::default()),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    path: String,
    /// 1-based first line.
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        super::READ_FILE
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: super::READ_FILE.to_string(),
            description: format!(
                "Read a regular text file up to {} bytes. Returns at most {MAX_LINES} lines or {}KB per call; use offset (1-based line \
                 number) and limit to page through larger files. Relative paths resolve against the working directory.",
                self.max_file_bytes,
                MAX_BYTES / 1024
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path" },
                    "offset": { "type": "integer", "minimum": 1, "description": "First line to read (1-based)" },
                    "limit": { "type": "integer", "minimum": 1, "description": "Maximum number of lines" }
                },
                "required": ["path"],
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
        let args: Args = parse_args(args)?;
        check_path(&args.path)
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, String> {
        let args: Args = parse_args(&args)?;
        let path = resolve(&ctx.cwd, &args.path)?;
        let read = unless_cancelled(ctx, self.executor.read_file(&path, self.max_file_bytes)).await;
        let bytes = read.ok_or_else(|| "read cancelled".to_string())??;
        if looks_binary(&bytes) {
            return Err(format!(
                "{} looks like a binary file; use bash with `file`, `xxd` or `strings` to inspect it",
                path.display()
            ));
        }

        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        let total = lines.len();
        if total == 0 {
            return Ok(ToolOutput::ok("(empty file)"));
        }
        let start = args.offset.unwrap_or(1).max(1);
        if start > total {
            return Err(format!("offset {start} is beyond the end of the file ({total} lines)"));
        }
        let end = args
            .limit
            .map_or(total, |limit| (start - 1).saturating_add(limit.max(1)).min(total));
        let selected = lines[start - 1..end].concat();
        let truncated = truncate_head(&selected, MAX_LINES, MAX_BYTES);
        let shown_end = start - 1 + truncated.kept_lines;
        // A first line over the byte limit is cut to a prefix but still counted as kept.
        // Paging is by line, so the note must say that this line is incomplete.
        let first_line = lines[start - 1].strip_suffix('\n').unwrap_or(lines[start - 1]);
        let cut_line = truncated.kept_lines == 1 && truncated.text.len() < first_line.len();
        let next = if shown_end < total {
            format!(" Use offset={} to continue.", shown_end + 1)
        } else {
            String::new()
        };
        let note = if cut_line {
            Some(format!(
                "[Line {start} of {total} was cut: showing {} of {} bytes. read_file cannot page within a line; \
                 use another tool, such as bash, to read the rest.{next}]",
                truncated.text.len(),
                first_line.len()
            ))
        } else if shown_end < total {
            Some(format!("[Showing lines {start}-{shown_end} of {total}.{next}]"))
        } else {
            None
        };

        let mut content = truncated.text;
        if let Some(note) = note {
            if !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(&note);
        }
        Ok(ToolOutput::ok(content))
    }
}

#[cfg(test)]
mod tests;
