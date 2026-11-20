//! `grep`, `find` and `ls` tools: read-only searches that need no bash command (pi:
//! coding-agent/src/core/tools/grep.ts, find.ts and ls.ts).
//!
//! They run system programs through the executor, so in a container they search the
//! container. ripgrep is used when it is installed, and then `.gitignore` is respected; pi
//! downloads it instead, which zx-agent does not do. Without it, the system's `grep` and
//! `find` do the work and skip only the version control directories. Every value from the model reaches
//! the shell single-quoted, so it is never interpreted.
//!
//! This module holds what the tools share: running a script and reading what it printed,
//! bounding and reporting the result, and the pieces of the scripts. Each tool is in a module
//! of its own.

mod find;
mod grep;
mod ls;

use std::time::Duration;

use serde_json::json;
use zx_core::tools::{ToolContext, ToolOutput};

pub use self::find::FindTool;
pub use self::grep::GrepTool;
pub use self::ls::LsTool;
use super::path_text;
use super::truncate::{MAX_BYTES, MAX_LINES, truncate_head, truncate_head_noted};
use crate::executor::{CAPTURE_LIMIT, CommandEnd, Executor, ShellCommand};
use crate::text::{prefix_noted, prefix_within};

/// Longest wait for one search: a search that takes longer is better narrowed than waited for,
/// and the error says so.
const TIMEOUT: Duration = Duration::from_secs(60);
/// Longest line shown, in bytes; longer matching lines are cut (pi: `GREP_MAX_LINE_LENGTH`).
const MAX_LINE_BYTES: usize = 500;
/// The marker a script writes to standard error to say which program searched.
const ENGINE_MARKER: &str = "zx-engine=";
/// The name the engine marker gives ripgrep; any other name is the system's program that
/// searched instead.
const RIPGREP: &str = "rg";
/// Directories of version control systems, skipped when `.gitignore` cannot be read.
const VCS_DIRECTORIES: [&str; 3] = [".git", ".hg", ".svn"];
/// The note after a line truncated to [`MAX_LINE_BYTES`].
const LINE_TRUNCATED: &str = " [line truncated]";

/// `text` as one single-quoted shell word: nothing in it is interpreted.
pub(super) fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The filter that keeps one line more than `shown`, so that a result over the limit is
/// noticed, and cuts every line to one byte more than is shown, so that a cut is noticed too.
/// [`MAX_LINES`] lines of this length stay under what the executor keeps of a command's
/// output, which keeps the end; the start, which a search shows, is never dropped. `cut`
/// counts bytes in the C locale: where it counts characters, it stops at the first line that
/// is not text in the user's encoding, and every line after it would be lost.
fn first_lines(shown: usize) -> String {
    format!("head -n {} | LC_ALL=C cut -c 1-{}", shown + 1, MAX_LINE_BYTES + 1)
}

// What `first_lines` keeps of a search that shows the most lines, each with its newline, fits
// in what the executor keeps.
const _: () = assert!((MAX_LINES + 1) * (MAX_LINE_BYTES + 2) <= CAPTURE_LIMIT);

/// The filter that drops the `./` before each path, as ripgrep and grep print the paths
/// under `.`, the directory they search by default. `sed` reads bytes in the C locale: where
/// it reads characters, macOS `sed` stops at a line that is not text in the user's encoding,
/// and prints nothing.
const WITHOUT_DOT_SLASH: &str = "LC_ALL=C sed 's|^\\./||'";

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

/// The output of one search script: its lines, which program searched, and the errors.
struct Searched {
    stdout: String,
    /// Standard error without the engine marker.
    stderr: String,
    /// The name of the program the script chose, from its marker: [`RIPGREP`] or the
    /// system's program.
    engine: Option<String>,
    /// Bytes the executor dropped because there were too many.
    dropped: usize,
}

/// Run `script` with `executor` in the call's working directory.
async fn search(executor: &dyn Executor, script: &str, ctx: &ToolContext) -> Result<Searched, String> {
    let command = ShellCommand {
        command: script,
        cwd: &ctx.cwd,
        timeout: TIMEOUT,
    };
    let output = executor.run(&command, &ctx.cancel).await?;
    match output.end {
        CommandEnd::Exited(_) => {}
        CommandEnd::TimedOut => {
            return Err(format!(
                "the search took longer than {TIMEOUT:?} and was stopped; narrow the path or the pattern"
            ));
        }
        CommandEnd::Cancelled => return Err("the search was cancelled".into()),
        CommandEnd::Signaled => return Err("the search was ended by a signal".into()),
        CommandEnd::WaitFailed(error) => return Err(format!("cannot wait for the search: {error}")),
    }
    let mut engine = None;
    // Lossily, like stdout: a stray byte must not hide the engine marker or an error.
    let stderr_text = String::from_utf8_lossy(&output.stderr.bytes);
    let mut stderr = Vec::new();
    for line in stderr_text.lines() {
        match line.strip_prefix(ENGINE_MARKER) {
            Some(name) => engine = Some(name.to_string()),
            None => stderr.push(line),
        }
    }
    Ok(Searched {
        stdout: String::from_utf8_lossy(&output.stdout.bytes).into_owned(),
        stderr: stderr.join("\n"),
        engine,
        dropped: output.stdout.dropped,
    })
}

/// Each line cut to [`MAX_LINE_BYTES`] at a character boundary, then the whole text bounded
/// like other tool output. Returns the text and whether anything was cut.
fn bound(text: &str) -> (String, bool) {
    let mut cut = false;
    let mut lines = String::with_capacity(text.len().min(MAX_BYTES * 2));
    for line in text.lines() {
        if line.len() > MAX_LINE_BYTES {
            lines.push_str(prefix_within(line, MAX_LINE_BYTES));
            lines.push_str(LINE_TRUNCATED);
            cut = true;
        } else {
            lines.push_str(line);
        }
        lines.push('\n');
    }
    let kept = truncate_head(&lines, MAX_LINES, MAX_BYTES);
    (kept.text, cut || kept.truncated)
}

/// The result text: the lines, then the `notes`, then the errors the programs reported
/// alongside their results.
fn report(searched: &Searched, lines: &str, mut notes: Vec<String>) -> ToolOutput {
    if !searched.stderr.trim().is_empty() {
        let first = searched.stderr.lines().next().unwrap_or_default();
        let first = prefix_noted(first, MAX_LINE_BYTES, |_, _| LINE_TRUNCATED.to_string());
        notes.push(format!("some paths could not be searched: {first}"));
    }
    let mut text = lines.to_string();
    for note in &notes {
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

/// What a search says about its result: when nothing was found, when not every line it
/// found is shown, and when lines were cut short.
#[derive(Clone, Copy)]
struct Wording {
    empty: &'static str,
    /// That more was found than is shown, such as "more files match".
    more: &'static str,
    /// What to do to see the rest.
    advice: &'static str,
    /// That lines too long to show whole were truncated, such as "long paths were truncated".
    long_lines: &'static str,
    /// What else the system's program left out, besides `.gitignore`, when it searched in
    /// place of ripgrep.
    also_not_applied: Option<&'static str>,
}

/// The result of a search that shows at most `shown` of its lines: the lines, then the notes
/// `wording` gives, then which program searched, what it left out and what it reported. A
/// note says how many lines are shown whenever some are not, by `shown` or by the bound of a
/// result.
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
    let kept = text.lines().count();
    let mut notes = Vec::new();
    if more || kept < all.len() {
        notes.push(format!(
            "{}; showing the first {kept} lines; {}",
            wording.more, wording.advice
        ));
    } else if cut {
        notes.push(wording.long_lines.to_string());
    }
    if let Some(program) = searched.engine.as_deref().filter(|name| *name != RIPGREP) {
        let not_applied = match wording.also_not_applied {
            Some(also) => format!(".gitignore and {also} were"),
            None => ".gitignore was".to_string(),
        };
        notes.push(format!(
            "ripgrep is not installed, so {program} searched and {not_applied} not applied"
        ));
    }
    Ok(report(searched, &text, notes))
}

/// The error when a search produced nothing but errors, bounded like a result.
fn failed(searched: &Searched) -> Option<String> {
    if !searched.stdout.trim().is_empty() || searched.stderr.trim().is_empty() {
        return None;
    }
    Some(truncate_head_noted(searched.stderr.trim(), "errors"))
}

/// The glob the model wrote as both programs take it: a leading `./` only names the search
/// directory, and the paths ripgrep matches a glob against do not start with it.
fn glob_of(raw: &str) -> &str {
    raw.trim().trim_start_matches("./")
}

/// `raw`, or `.` when absent or blank, read as read_file reads a path.
fn search_path(raw: Option<&str>) -> String {
    // An absent or blank path is the working directory.
    path_text(raw.unwrap_or_default()).unwrap_or(".").into()
}

/// The limit asked for, at most [`MAX_LINES`], which is all a result can show anyway, or
/// `default` when none was. Zero is refused.
fn limit_or(limit: Option<usize>, default: usize) -> Result<usize, String> {
    match limit {
        Some(0) => Err(format!(
            "limit must be at least 1; omit it to use the default of {default}"
        )),
        Some(limit) => Ok(limit.min(MAX_LINES)),
        None => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use async_trait::async_trait;
    use tokio_util::sync::CancellationToken;
    use zx_core::tools::Tool;

    use super::*;
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
            executor: Arc::new(Scripted {
                stdout: stdout.to_vec(),
                stderr: stderr.to_vec(),
            }),
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

    #[cfg(unix)]
    #[test]
    fn the_line_filter_keeps_the_lines_after_one_that_is_not_utf8() {
        // Where `cut` counts characters, it stops at such a line unless it is told to count bytes.
        let script = format!("printf 'one\\n\\351two\\nthree\\n' | {}", first_lines(10));
        let output = std::process::Command::new("sh")
            .args(["-c", &script])
            .env("LC_ALL", "en_US.UTF-8")
            .output()
            .unwrap();
        assert_eq!(output.stdout, b"one\n\xe9two\nthree\n", "{output:?}");
    }

    #[cfg(unix)]
    #[test]
    fn the_dot_filter_keeps_the_lines_after_one_that_is_not_utf8() {
        let script = format!("printf './one\\n./\\351two\\n./three\\n' | {WITHOUT_DOT_SLASH}");
        let output = std::process::Command::new("sh")
            .args(["-c", &script])
            .env("LC_ALL", "en_US.UTF-8")
            .output()
            .unwrap();
        assert_eq!(output.stdout, b"one\n\xe9two\nthree\n", "{output:?}");
    }

    #[tokio::test]
    async fn the_note_on_grep_in_place_of_ripgrep_says_what_it_left_out() {
        let tool = grep_with(b"zx-engine=grep\n");
        let names = tool
            .execute(json!({"pattern": "hit", "glob": "*.rs"}), &ctx())
            .await
            .unwrap();
        assert!(
            names
                .content
                .contains("so grep searched and .gitignore was not applied]"),
            "{}",
            names.content
        );
        let paths = tool
            .execute(json!({"pattern": "hit", "glob": "src/*.rs"}), &ctx())
            .await
            .unwrap();
        assert!(
            paths
                .content
                .contains("so grep searched and .gitignore and the directories of the glob were not applied]"),
            "{}",
            paths.content
        );
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
        assert!(output.content.contains(LINE_TRUNCATED), "{}", output.content);
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
        assert!(second.ends_with(LINE_TRUNCATED), "{second}");
        assert!(second.len() <= MAX_LINE_BYTES + LINE_TRUNCATED.len());
        assert_eq!(bound("a\nb\n"), ("a\nb\n".to_string(), false));
    }

    #[test]
    fn limits_are_capped_at_what_a_result_can_show() {
        assert_eq!(limit_or(Some(1_000_000), 10), Ok(MAX_LINES));
        assert_eq!(limit_or(None, 10), Ok(10));
    }
}
