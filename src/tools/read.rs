//! `read_file` tool: read a text file in pages.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::truncate::{MAX_BYTES, MAX_LINES, truncate_head};
use crate::agent::{Effects, Tool, ToolContext, ToolOutput};
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
            max_file_bytes: 10 * 1024 * 1024,
            executor: Arc::new(LocalExecutor),
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

    fn effects(&self) -> Effects {
        Effects::ReadOnly
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

    fn validate_arguments(&self, args: &Value) -> Result<(), String> {
        let args = Args::deserialize(args).map_err(|e| format!("invalid arguments: {e}"))?;
        resolve(Path::new("."), &args.path).map(|_| ())
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, String> {
        let args: Args = serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))?;
        let path = resolve(&ctx.cwd, &args.path)?;
        let bytes = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return Err("read cancelled".to_string()),
            result = self.executor.read_file(&path, self.max_file_bytes) => result?,
        };
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
    async fn a_cancelled_read_does_not_return_file_contents() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "must not be returned").unwrap();
        let context = ctx(dir.path());
        context.cancel.cancel();

        let result = ReadFileTool::default()
            .execute(json!({"path": "f.txt"}), &context)
            .await;

        assert!(result.is_err(), "a cancelled read returned {result:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refuses_special_files() {
        let result = ReadFileTool::default()
            .execute(json!({"path": "/dev/null"}), &ctx(Path::new("/")))
            .await;

        assert!(result.is_err(), "a special file was accepted: {result:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refuses_named_pipes() {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fifo");
        assert!(
            tokio::process::Command::new("mkfifo")
                .arg(&path)
                .status()
                .await
                .unwrap()
                .success()
        );
        let writer_path = path.clone();
        // This writer also lets the old implementation complete, so a failing regression
        // test never leaves a blocking read behind during runtime shutdown.
        let writer = tokio::task::spawn_blocking(move || {
            let mut file = std::fs::OpenOptions::new().write(true).open(writer_path).unwrap();
            file.write_all(b"pipe contents")
        });
        let result = ReadFileTool::default()
            .execute(json!({"path": "fifo"}), &ctx(dir.path()))
            .await;
        // Release the writer if the reader rejected the FIFO before opening it.
        let _unblock = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .unwrap();
        // Broken pipe is expected when rejection closes the read descriptor first.
        let _ = writer.await.unwrap();
        assert!(result.unwrap_err().contains("not a regular file"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refusing_a_named_pipe_does_not_open_it() {
        use std::os::unix::fs::OpenOptionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fifo");
        assert!(
            tokio::process::Command::new("mkfifo")
                .arg(&path)
                .status()
                .await
                .unwrap()
                .success()
        );
        let writer_path = path.clone();
        // Opening a FIFO for writing waits until something opens it for reading: the writer
        // is released only if the reader opens the FIFO it refuses, as opening a device would
        // set off whatever the device does on open.
        let mut writer =
            tokio::task::spawn_blocking(move || std::fs::OpenOptions::new().write(true).open(writer_path).map(drop));
        let result = ReadFileTool::default()
            .execute(json!({"path": "fifo"}), &ctx(dir.path()))
            .await;
        // Showing that the writer stays blocked needs a bounded wait. A reader that does not
        // open the FIFO never releases it, so the wait cannot fail a correct implementation.
        let released = tokio::time::timeout(std::time::Duration::from_millis(300), &mut writer)
            .await
            .is_ok();
        if !released {
            let _unblock = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&path)
                .unwrap();
            writer.await.unwrap().unwrap();
        }
        assert!(result.unwrap_err().contains("not a regular file"));
        assert!(!released, "refusing the FIFO opened it");
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
    async fn a_line_over_the_byte_limit_is_reported_as_cut() {
        let dir = tempfile::tempdir().unwrap();
        // One line, as in minified JSON. The byte limit falls inside a 3-byte character.
        let line = "界".repeat(MAX_BYTES / 3 + 10);
        std::fs::write(dir.path().join("one.json"), &line).unwrap();
        let output = ReadFileTool::default()
            .execute(json!({"path": "one.json"}), &ctx(dir.path()))
            .await
            .unwrap();
        let (shown, note) = output
            .content
            .rsplit_once('\n')
            .expect("a cut line must be followed by a note");
        assert!(
            line.starts_with(shown) && shown.len() <= MAX_BYTES,
            "{} bytes shown",
            shown.len()
        );
        assert_eq!(
            note,
            format!(
                "[Line 1 of 1 was cut: showing {} of {} bytes. read_file cannot page within a line; \
                 use another tool, such as bash, to read the rest.]",
                shown.len(),
                line.len()
            )
        );
    }

    #[tokio::test]
    async fn a_cut_line_is_not_counted_as_shown_when_more_lines_follow() {
        let dir = tempfile::tempdir().unwrap();
        let long = "x".repeat(MAX_BYTES + 100);
        std::fs::write(dir.path().join("two.txt"), format!("{long}\nnext\n")).unwrap();
        let output = ReadFileTool::default()
            .execute(json!({"path": "two.txt"}), &ctx(dir.path()))
            .await
            .unwrap();
        assert!(output.content.starts_with(&long[..MAX_BYTES]));
        assert_eq!(
            &output.content[MAX_BYTES..],
            format!(
                "\n[Line 1 of 2 was cut: showing {MAX_BYTES} of {} bytes. read_file cannot page within a line; \
                 use another tool, such as bash, to read the rest. Use offset=2 to continue.]",
                long.len()
            )
        );
    }

    #[tokio::test]
    async fn a_line_that_fits_the_byte_limit_is_not_reported_as_cut() {
        let dir = tempfile::tempdir().unwrap();
        let line = "y".repeat(MAX_BYTES);
        std::fs::write(dir.path().join("fits.txt"), format!("{line}\nnext\n")).unwrap();
        let output = ReadFileTool::default()
            .execute(json!({"path": "fits.txt"}), &ctx(dir.path()))
            .await
            .unwrap();
        assert!(output.content.starts_with(&line));
        assert_eq!(
            &output.content[MAX_BYTES..],
            "\n[Showing lines 1-1 of 2. Use offset=2 to continue.]"
        );
    }

    #[tokio::test]
    async fn refuses_files_over_the_size_limit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("big.txt"), "hello\n").unwrap();
        let tool = ReadFileTool {
            max_file_bytes: 4,
            ..ReadFileTool::default()
        };
        let error = tool
            .execute(json!({"path": "big.txt"}), &ctx(dir.path()))
            .await
            .unwrap_err();
        assert!(error.contains("over the 4 byte limit"), "{error}");
    }

    #[tokio::test]
    async fn reads_exact_size_limits_and_large_page_limits_without_overflow() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("exact.txt"), "ab\nc").unwrap();
        let tool = ReadFileTool {
            max_file_bytes: 4,
            ..ReadFileTool::default()
        };
        let output = tool
            .execute(
                json!({"path": "exact.txt", "offset": 2, "limit": usize::MAX}),
                &ctx(dir.path()),
            )
            .await
            .unwrap();
        assert_eq!(output.content, "c");
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
