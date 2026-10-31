//! `read_file` tool: read a text file in pages.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::truncate::{MAX_BYTES, MAX_LINES, truncate_head};
use crate::agent::{Tool, ToolContext, ToolOutput};
use crate::ai::ToolDef;

/// Reads text files in pages of bounded size.
pub struct ReadFileTool {
    /// Larger files are refused; the model should use `bash` with head, tail, or grep.
    pub max_file_bytes: u64,
}

impl Default for ReadFileTool {
    fn default() -> Self {
        Self {
            max_file_bytes: 10 * 1024 * 1024,
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
        "read_file"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "read_file".to_string(),
            description: format!(
                "Read a text file. Returns at most {MAX_LINES} lines or {}KB per call; use offset (1-based line \
                 number) and limit to page through larger files. Relative paths resolve against the working directory.",
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

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, String> {
        let args: Args = serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))?;
        let path = resolve(&ctx.cwd, &args.path)?;
        let meta = tokio::fs::metadata(&path)
            .await
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if meta.is_dir() {
            return Err(format!("{} is a directory; use bash `ls` to list it", path.display()));
        }
        if meta.len() > self.max_file_bytes {
            return Err(format!(
                "{} is {} bytes, over the {} byte limit; use bash with head, tail, or grep instead",
                path.display(),
                meta.len(),
                self.max_file_bytes
            ));
        }
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if bytes.iter().take(8192).any(|&b| b == 0) {
            return Err(format!("{} looks like a binary file", path.display()));
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
        let end = args.limit.map_or(total, |limit| (start - 1 + limit.max(1)).min(total));
        let selected = lines[start - 1..end].concat();
        let truncated = truncate_head(&selected, MAX_LINES, MAX_BYTES);
        let shown_end = start - 1 + truncated.kept_lines;

        let mut content = truncated.text;
        if shown_end < total {
            if !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(&format!(
                "[Showing lines {start}-{shown_end} of {total}. Use offset={} to continue.]",
                shown_end + 1
            ));
        }
        Ok(ToolOutput::ok(content))
    }
}

fn resolve(cwd: &Path, raw: &str) -> Result<PathBuf, String> {
    // Some models prefix paths with '@'; pi's built-in tools strip it too.
    let raw = raw.trim();
    let raw = raw.strip_prefix('@').unwrap_or(raw);
    if raw.is_empty() {
        return Err("path is empty".to_string());
    }
    let path = Path::new(raw);
    Ok(if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn ctx(dir: &Path) -> ToolContext {
        ToolContext {
            cwd: dir.to_path_buf(),
            cancel: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn pages_through_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let content: String = (1..=10).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.path().join("f.txt"), content).unwrap();
        let tool = ReadFileTool::default();

        let page = tool
            .execute(json!({"path": "@f.txt", "offset": 3, "limit": 2}), &ctx(dir.path()))
            .await
            .unwrap();
        assert_eq!(
            page.content,
            "line 3\nline 4\n[Showing lines 3-4 of 10. Use offset=5 to continue.]"
        );

        let rest = tool
            .execute(json!({"path": "f.txt", "offset": 9}), &ctx(dir.path()))
            .await
            .unwrap();
        assert_eq!(rest.content, "line 9\nline 10\n");
    }

    #[tokio::test]
    async fn empty_files_say_so() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("e.txt"), "").unwrap();
        let output = ReadFileTool::default()
            .execute(json!({"path": "e.txt"}), &ctx(dir.path()))
            .await
            .unwrap();
        assert_eq!(output.content, "(empty file)");
    }

    #[tokio::test]
    async fn long_files_end_with_a_hint_to_continue() {
        let dir = tempfile::tempdir().unwrap();
        let content: String = (1..=MAX_LINES + 5).map(|i| format!("{i}\n")).collect();
        std::fs::write(dir.path().join("long.txt"), content).unwrap();
        let output = ReadFileTool::default()
            .execute(json!({"path": "long.txt"}), &ctx(dir.path()))
            .await
            .unwrap();
        let expected = format!(
            "[Showing lines 1-{MAX_LINES} of {}. Use offset={} to continue.]",
            MAX_LINES + 5,
            MAX_LINES + 1
        );
        assert!(
            output.content.ends_with(&expected),
            "{}",
            &output.content[output.content.len() - 80..]
        );
    }

    #[tokio::test]
    async fn refuses_files_over_the_size_limit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("big.txt"), "hello\n").unwrap();
        let tool = ReadFileTool { max_file_bytes: 4 };
        let error = tool
            .execute(json!({"path": "big.txt"}), &ctx(dir.path()))
            .await
            .unwrap_err();
        assert!(error.contains("over the 4 byte limit"), "{error}");
    }

    #[tokio::test]
    async fn rejects_unknown_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let error = ReadFileTool::default()
            .execute(json!({"path": "x", "lines": 3}), &ctx(dir.path()))
            .await
            .unwrap_err();
        assert!(error.contains("unknown field"), "{error}");
    }

    #[tokio::test]
    async fn refuses_directories_binaries_and_bad_offsets() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.bin"), [0u8, 1, 2]).unwrap();
        std::fs::write(dir.path().join("t.txt"), "a\n").unwrap();
        let tool = ReadFileTool::default();
        let c = ctx(dir.path());
        assert!(
            tool.execute(json!({"path": "."}), &c)
                .await
                .unwrap_err()
                .contains("directory")
        );
        assert!(
            tool.execute(json!({"path": "b.bin"}), &c)
                .await
                .unwrap_err()
                .contains("binary")
        );
        assert!(
            tool.execute(json!({"path": "t.txt", "offset": 5}), &c)
                .await
                .unwrap_err()
                .contains("beyond")
        );
        assert!(tool.execute(json!({"path": "missing"}), &c).await.is_err());
    }
}
