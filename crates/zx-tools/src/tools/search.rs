//! `grep`, `find` and `ls` tools: read-only searches that need no command allowlist (pi:
//! coding-agent/src/core/tools/grep.ts, find.ts and ls.ts).
//!
//! They run system programs through the executor, so in a container they search the
//! container. ripgrep is used when it is installed, and then `.gitignore` is respected; pi
//! downloads it instead, which zx-agent does not do. Without it, POSIX `grep` and `find` do
//! the work and skip only the version control directories. Every value from the model reaches
//! the shell single-quoted, so it is never interpreted.
//!
//! This module holds what the tools share: their settings, running a script and reading what
//! it printed, bounding and reporting the result, and the pieces of the scripts. Each tool is
//! in a module of its own.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use super::path_text;
use super::truncate::{MAX_BYTES, MAX_LINES, truncate_head, truncate_head_noted};
use crate::agent::{ToolContext, ToolOutput};
use crate::executor::{CommandEnd, Executor, LocalExecutor, ShellCommand};
use crate::text::{prefix_noted, prefix_within};

/// Longest wait for one search: a search that takes longer is better narrowed than waited for,
/// and the error says so.
const TIMEOUT: Duration = Duration::from_secs(60);
/// Longest line shown, in bytes; longer matching lines are cut (pi: `GREP_MAX_LINE_LENGTH`).
const MAX_LINE_BYTES: usize = 500;
/// The marker a script writes to standard error to say which program searched.
const ENGINE_MARKER: &str = "zx-engine=";
/// The name the engine marker gives ripgrep; any other name is the POSIX program that searched
/// instead.
const RIPGREP: &str = "rg";
/// Directories of version control systems, skipped when `.gitignore` cannot be read.
const VCS_DIRECTORIES: [&str; 3] = [".git", ".hg", ".svn"];
/// The note after a line cut to [`MAX_LINE_BYTES`].
const LINE_CUT: &str = " [line cut]";

/// Shared tool plumbing: read-only, and running one at a time when the executor does.
macro_rules! search_tool_basics {
    ($name:expr) => {
        fn name(&self) -> &str {
            $name
        }

        fn execution_mode(&self) -> $crate::agent::ExecutionMode {
            self.settings.executor.execution_mode()
        }

        fn effects(&self) -> $crate::agent::Effects {
            $crate::agent::Effects::ReadOnly
        }
    };
}

// The tools use the macro, which is in scope only after its definition.
mod find;
mod grep;
mod ls;

pub use find::FindTool;
pub use grep::GrepTool;
pub use ls::LsTool;

/// `text` as one single-quoted shell word: nothing in it is interpreted.
pub(crate) fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The filter that keeps one line more than `shown`, so that a result over the limit is
/// noticed, and cuts every line to one byte more than is shown, so that a cut is noticed too.
/// [`MAX_LINES`] lines of this length stay under what the executor keeps of a command's
/// output, which keeps the end; the start, which a search shows, is never dropped.
fn first_lines(shown: usize) -> String {
    format!("head -n {} | cut -c 1-{}", shown + 1, MAX_LINE_BYTES + 1)
}

/// The script that searches with `rg` when ripgrep is installed and with `fallback`, the
/// program named `name`, otherwise; each announces itself with the engine marker on
/// standard error.
fn choose_engine(rg: &str, name: &str, fallback: &str) -> String {
    format!(
        "if command -v rg >/dev/null 2>&1; then echo {ENGINE_MARKER}{RIPGREP} >&2; {rg}; \
         else echo {ENGINE_MARKER}{name} >&2; {fallback}; fi"
    )
}

/// ripgrep's exclusion of the version control directories. It goes after the model's glob:
/// in ripgrep a later glob overrides an earlier one.
fn ripgrep_excludes() -> String {
    VCS_DIRECTORIES
        .iter()
        .map(|directory| format!("--glob '!{directory}'"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// What the three tools share: where they run.
#[derive(Clone)]
pub struct SearchSettings {
    /// Where the programs run, and so what they must not see: on this machine, the variables
    /// in [`LocalExecutor::hidden_env`]. Defaults to this machine, hiding nothing.
    pub executor: Arc<dyn Executor>,
}

impl Default for SearchSettings {
    fn default() -> Self {
        Self {
            executor: Arc::new(LocalExecutor::default()),
        }
    }
}

/// The output of one search script: its lines, which program searched, and the errors.
struct Searched {
    stdout: String,
    /// Standard error without the engine marker.
    stderr: String,
    /// The name of the program the script chose, from its marker: [`RIPGREP`] or a POSIX
    /// program.
    engine: Option<String>,
    /// Bytes the executor dropped because there were too many.
    dropped: usize,
}

impl SearchSettings {
    /// Run `script` with `sh -c` in the call's working directory.
    async fn run(&self, script: &str, ctx: &ToolContext) -> Result<Searched, String> {
        let command = ShellCommand {
            shell: "sh",
            command: script,
            cwd: &ctx.cwd,
            timeout: TIMEOUT,
        };
        let output = self.executor.run(&command, &ctx.cancel).await?;
        match output.end {
            CommandEnd::Exited(_) => {}
            CommandEnd::TimedOut => {
                return Err(format!(
                    "the search took longer than {}s and was stopped; narrow the path or the pattern",
                    TIMEOUT.as_secs()
                ));
            }
            CommandEnd::Cancelled => return Err("the search was cancelled".into()),
            CommandEnd::Signaled => return Err("the search was ended by a signal".into()),
            CommandEnd::WaitFailed(error) => return Err(format!("the search could not be waited for: {error}")),
        }
        let mut engine = None;
        // Lossily, like stdout: a stray byte must not hide the engine marker or an error.
        let stderr_text = String::from_utf8_lossy(&output.stderr.bytes);
        let stderr: Vec<&str> = stderr_text
            .lines()
            .filter(|line| match line.strip_prefix(ENGINE_MARKER) {
                Some(name) => {
                    engine = Some(name.to_string());
                    false
                }
                None => true,
            })
            .collect();
        Ok(Searched {
            stdout: String::from_utf8_lossy(&output.stdout.bytes).into_owned(),
            stderr: stderr.join("\n"),
            engine,
            dropped: output.stdout.dropped,
        })
    }
}

/// Each line cut to [`MAX_LINE_BYTES`] at a character boundary, then the whole text bounded
/// like other tool output. Returns the text and whether anything was cut.
fn bound(text: &str) -> (String, bool) {
    let mut cut = false;
    let mut lines = String::with_capacity(text.len().min(MAX_BYTES * 2));
    for line in text.lines() {
        if line.len() > MAX_LINE_BYTES {
            lines.push_str(prefix_within(line, MAX_LINE_BYTES));
            lines.push_str(LINE_CUT);
            cut = true;
        } else {
            lines.push_str(line);
        }
        lines.push('\n');
    }
    let kept = truncate_head(&lines, MAX_LINES, MAX_BYTES);
    (kept.text, cut || kept.truncated)
}

/// The result text: the lines, then notes on what was cut, which program searched and the
/// errors the programs reported alongside their results.
fn report(searched: &Searched, lines: &str, notes: &mut Vec<String>) -> ToolOutput {
    if let Some(program) = searched.engine.as_deref().filter(|name| *name != RIPGREP) {
        notes.push(format!(
            "ripgrep is not installed, so {program} searched and .gitignore was not applied"
        ));
    }
    if !searched.stderr.trim().is_empty() {
        let first = searched.stderr.lines().next().unwrap_or_default();
        let first = prefix_noted(first, MAX_LINE_BYTES, |_, _| LINE_CUT.to_string());
        notes.push(format!("some paths could not be searched: {first}"));
    }
    let mut text = lines.to_string();
    for note in notes.iter() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!("[{note}]\n"));
    }
    let output = ToolOutput::ok(text);
    match &searched.engine {
        Some(engine) => output.with_details(json!({ "engine": engine })),
        None => output,
    }
}

/// What a search says about its result: when nothing was found, when more exists than is
/// shown, and when lines were cut (`None`: cuts go unmentioned).
struct Wording {
    empty: &'static str,
    more: String,
    cut: Option<&'static str>,
}

/// The result of a search that shows at most `shown` of its lines: the lines, then the notes
/// `wording` gives, then which program searched and what it reported.
fn finish(searched: &Searched, shown: usize, wording: Wording) -> Result<ToolOutput, String> {
    if let Some(error) = failed(searched) {
        return Err(error);
    }
    if searched.stdout.trim().is_empty() {
        return Ok(ToolOutput::ok(wording.empty));
    }
    let mut all: Vec<&str> = searched.stdout.lines().collect();
    let more = all.len() > shown || searched.dropped > 0;
    all.truncate(shown);
    let (text, cut) = bound(&all.join("\n"));
    let mut notes = Vec::new();
    if more {
        notes.push(wording.more);
    } else if let Some(note) = wording.cut.filter(|_| cut) {
        notes.push(note.to_string());
    }
    Ok(report(searched, &text, &mut notes))
}

/// The error when a search produced nothing but errors, bounded like a result.
fn failed(searched: &Searched) -> Option<String> {
    if !searched.stdout.trim().is_empty() || searched.stderr.trim().is_empty() {
        return None;
    }
    Some(truncate_head_noted(searched.stderr.trim(), "errors"))
}

/// `raw`, or `.` when absent or blank, read as read_file reads a path.
fn search_path(raw: Option<&str>) -> String {
    let raw = path_text(raw.unwrap_or_default());
    if raw.is_empty() { ".".into() } else { raw.into() }
}

/// The limit asked for, at most [`MAX_LINES`], which is all a result can show anyway.
fn positive(limit: Option<usize>, default: usize, name: &str) -> Result<usize, String> {
    match limit {
        Some(0) => Err(format!("{name} must be at least 1; omit it for {default}")),
        Some(limit) => Ok(limit.min(MAX_LINES)),
        None => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use async_trait::async_trait;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::agent::Tool;
    use crate::executor::{Captured, CommandOutput};

    /// An executor whose searches print `stdout` and `stderr` as given and exit with 0.
    struct Scripted {
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    }

    #[async_trait]
    impl Executor for Scripted {
        async fn run(&self, _: &ShellCommand<'_>, _: &CancellationToken) -> Result<CommandOutput, String> {
            Ok(CommandOutput {
                stdout: Captured {
                    bytes: self.stdout.clone(),
                    dropped: 0,
                },
                stderr: Captured {
                    bytes: self.stderr.clone(),
                    dropped: 0,
                },
                end: CommandEnd::Exited(0),
            })
        }

        async fn read_file(&self, _: &Path, _: u64) -> Result<Vec<u8>, String> {
            Err("not a file executor".into())
        }

        async fn write_file(&self, _: &Path, _: &[u8]) -> Result<(), String> {
            Err("not a file executor".into())
        }
    }

    /// A grep whose search finds one line and prints `stderr` as given.
    fn grep_with(stderr: &[u8]) -> GrepTool {
        grep_printing(b"src/a.rs:1:hit\n", stderr)
    }

    fn grep_printing(stdout: &[u8], stderr: &[u8]) -> GrepTool {
        GrepTool {
            settings: SearchSettings {
                executor: Arc::new(Scripted {
                    stdout: stdout.to_vec(),
                    stderr: stderr.to_vec(),
                }),
            },
        }
    }

    fn ctx() -> ToolContext {
        ToolContext::new(PathBuf::from("."), CancellationToken::new())
    }

    #[tokio::test]
    async fn the_engine_marker_is_read_from_error_output_that_is_not_utf8() {
        let tool = grep_with(b"zx-engine=grep\n\xff");
        let output = tool.execute(json!({"pattern": "hit"}), &ctx()).await.unwrap();
        assert_eq!(output.details.unwrap()["engine"], "grep", "{}", output.content);
    }

    #[tokio::test]
    async fn a_search_that_only_produced_errors_reports_them_bounded() {
        // Many unreadable files: the executor keeps up to a mebibyte of such lines.
        let line = "grep: /var/log/x: Permission denied\n";
        let count = MAX_BYTES * 3 / line.len();
        let tool = grep_printing(b"", line.repeat(count).as_bytes());
        let error = tool.execute(json!({"pattern": "hit"}), &ctx()).await.unwrap_err();
        assert!(error.starts_with(line), "{}", &error[..line.len()]);
        assert!(error.len() <= MAX_BYTES + 100, "{} bytes", error.len());
        assert!(
            error.ends_with(&format!("of {count} lines]")),
            "{}",
            &error[error.len() - 60..]
        );
    }

    #[tokio::test]
    async fn the_error_noted_beside_results_is_cut_like_a_line() {
        let stderr = format!("grep: {}: Permission denied", "x".repeat(MAX_BYTES));
        let tool = grep_with(stderr.as_bytes());
        let output = tool.execute(json!({"pattern": "hit"}), &ctx()).await.unwrap();
        assert!(output.content.starts_with("src/a.rs:1:hit\n"), "{}", output.content);
        assert!(output.content.contains(LINE_CUT), "{}", output.content);
        assert!(
            output.content.len() <= MAX_LINE_BYTES + 100,
            "{} bytes",
            output.content.len()
        );
    }

    #[test]
    fn quoting_leaves_nothing_for_the_shell_to_interpret() {
        assert_eq!(quote("plain"), "'plain'");
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(quote(""), "''");
    }

    #[tokio::test]
    async fn hostile_values_reach_the_programs_as_plain_text() {
        let dir = tempfile::tempdir().unwrap();
        for value in [
            "$(touch pwned)",
            "`touch pwned`",
            "'; touch pwned; '",
            "a\nb; touch pwned",
            "\\'; touch pwned #",
        ] {
            let script = format!("printf '%s' {}", quote(value));
            let output = tokio::process::Command::new("sh")
                .args(["-c", &script])
                .current_dir(dir.path())
                .output()
                .await
                .unwrap();
            assert_eq!(String::from_utf8(output.stdout).unwrap(), value);
            assert!(!dir.path().join("pwned").exists(), "{value:?} ran a command");
        }
    }

    #[test]
    fn long_lines_are_cut_at_a_character_boundary() {
        let line = "界".repeat(MAX_LINE_BYTES);
        let (text, cut) = bound(&format!("short\n{line}\n"));
        assert!(cut);
        let second = text.lines().nth(1).unwrap();
        assert!(second.ends_with(LINE_CUT), "{second}");
        assert!(second.len() <= MAX_LINE_BYTES + LINE_CUT.len());
        assert_eq!(bound("a\nb\n"), ("a\nb\n".to_string(), false));
    }

    #[test]
    fn limits_are_capped_at_what_a_result_can_show() {
        assert_eq!(positive(Some(1_000_000), 10, "limit"), Ok(MAX_LINES));
        assert_eq!(positive(None, 10, "limit"), Ok(10));
    }
}
