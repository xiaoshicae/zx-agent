//! `grep`, `find` and `ls` tools: read-only searches that need no command allowlist (pi:
//! coding-agent/src/core/tools/grep.ts, find.ts and ls.ts).
//!
//! They run system programs through the executor, so in a container they search the
//! container. ripgrep is used when it is installed, and then `.gitignore` is respected; pi
//! downloads it instead, which zx-agent does not do. Without it, POSIX `grep` and `find` do
//! the work and skip only the version control directories. Every value from the model reaches
//! the shell single-quoted, so it is never interpreted.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::parse_args;
use super::truncate::{MAX_BYTES, MAX_LINES, truncate_head};
use crate::agent::{Effects, ExecutionMode, Tool, ToolContext, ToolOutput};
use crate::ai::ToolDef;
use crate::executor::{CommandEnd, Executor, LocalExecutor, ShellCommand};
use crate::text::prefix_within;

/// Longest wait for one search.
const TIMEOUT: Duration = Duration::from_secs(60);
/// Longest line shown, in bytes; longer matching lines are cut (pi: `GREP_MAX_LINE_LENGTH`).
const MAX_LINE_BYTES: usize = 500;
/// The marker a script writes to standard error to say which program searched.
const ENGINE_MARKER: &str = "zx-engine=";
/// Directories of version control systems, skipped when `.gitignore` cannot be read.
const VCS_DIRECTORIES: [&str; 3] = [".git", ".hg", ".svn"];

/// `text` as one single-quoted shell word: nothing in it is interpreted.
pub(crate) fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The filter that cuts every output line to one byte more than shown, so that the cut is
/// still noticed. [`MAX_LINES`] lines of this length stay under what the executor keeps of a
/// command's output, which keeps the end; the start, which a search shows, is never dropped.
fn cut_command() -> String {
    format!("cut -c 1-{}", MAX_LINE_BYTES + 1)
}

/// What the three tools share: where they run and what commands must not see.
#[derive(Clone)]
pub struct SearchSettings {
    /// Where the programs run. Defaults to this machine.
    pub executor: Arc<dyn Executor>,
    /// Environment variables the programs must not see, such as the API key variable.
    pub hidden_env: Vec<String>,
    /// Longest wait for one call.
    pub timeout: Duration,
}

impl Default for SearchSettings {
    fn default() -> Self {
        Self {
            executor: Arc::new(LocalExecutor),
            hidden_env: Vec::new(),
            timeout: TIMEOUT,
        }
    }
}

/// The output of one search script: its lines, which program searched, and the errors.
struct Searched {
    stdout: String,
    /// Standard error without the engine marker.
    stderr: String,
    /// The program the script chose, from its marker.
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
            timeout: self.timeout,
            hidden_env: &self.hidden_env,
        };
        let output = self.executor.run(&command, &ctx.cancel).await?;
        match output.end {
            CommandEnd::Exited(_) => {}
            CommandEnd::TimedOut => {
                return Err(format!(
                    "the search took longer than {}s and was stopped; narrow the path or the pattern",
                    self.timeout.as_secs()
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

/// Shared tool plumbing: read-only, and running one at a time when the executor does.
macro_rules! search_tool_basics {
    ($name:expr) => {
        fn name(&self) -> &str {
            $name
        }

        fn execution_mode(&self) -> ExecutionMode {
            self.settings.executor.execution_mode()
        }

        fn effects(&self) -> Effects {
            Effects::ReadOnly
        }
    };
}

/// Each line cut to [`MAX_LINE_BYTES`] at a character boundary, then the whole text bounded
/// like other tool output. Returns the text and whether anything was cut.
fn bound(text: &str) -> (String, bool) {
    let mut cut = false;
    let mut lines = String::with_capacity(text.len().min(MAX_BYTES * 2));
    for line in text.lines() {
        if line.len() > MAX_LINE_BYTES {
            lines.push_str(prefix_within(line, MAX_LINE_BYTES));
            lines.push_str(" [line cut]");
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
    if searched.engine.as_deref() != Some("rg")
        && let Some(engine) = &searched.engine
    {
        notes.push(format!(
            "ripgrep is not installed, so {engine} searched and .gitignore was not applied"
        ));
    }
    if !searched.stderr.trim().is_empty() {
        let first = searched.stderr.lines().next().unwrap_or_default();
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

/// The error when a search produced nothing but errors.
fn failed(searched: &Searched) -> Option<String> {
    (searched.stdout.trim().is_empty() && !searched.stderr.trim().is_empty())
        .then(|| searched.stderr.trim().to_string())
}

/// `raw`, or `.` when absent or blank, without a leading `@` (as read_file accepts).
fn search_path(raw: Option<&str>) -> String {
    let raw = raw.map(str::trim).unwrap_or_default();
    let raw = raw.strip_prefix('@').unwrap_or(raw);
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

// ---------------------------------------------------------------------------------------
// grep
// ---------------------------------------------------------------------------------------

/// Matches shown by default (pi: grep's `DEFAULT_LIMIT`).
const GREP_LIMIT: usize = 100;
/// The most context lines on each side.
const MAX_CONTEXT: usize = 10;

/// Searches file contents for a pattern.
#[derive(Default)]
pub struct GrepTool {
    pub settings: SearchSettings,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    ignore_case: bool,
    #[serde(default)]
    literal: bool,
    #[serde(default)]
    context: usize,
    #[serde(default)]
    limit: Option<usize>,
}

impl GrepArgs {
    fn parse(args: &Value) -> Result<Self, String> {
        let args: Self = parse_args(args)?;
        if args.pattern.is_empty() {
            return Err("pattern is empty".into());
        }
        if args.context > MAX_CONTEXT {
            return Err(format!("context must be at most {MAX_CONTEXT}"));
        }
        positive(args.limit, GREP_LIMIT, "limit")?;
        Ok(args)
    }

    /// The shell script: ripgrep when installed, otherwise POSIX grep, each line cut, at most
    /// `lines` lines.
    fn script(&self, lines: usize) -> String {
        let path = quote(&search_path(self.path.as_deref()));
        let pattern = quote(&self.pattern);
        let mut rg =
            vec!["rg --no-config --line-number --with-filename --no-heading --color=never --hidden".to_string()];
        let mut grep = vec!["grep -rnHI".to_string()];
        if self.ignore_case {
            rg.push("--ignore-case".into());
            grep.push("-i".into());
        }
        if self.literal {
            rg.push("--fixed-strings".into());
            grep.push("-F".into());
        } else {
            grep.push("-E".into());
        }
        if let Some(glob) = self.glob.as_deref().filter(|glob| !glob.trim().is_empty()) {
            rg.push(format!("--glob {}", quote(glob)));
            // grep matches --include against file names only.
            let name = glob.rsplit('/').next().unwrap_or(glob);
            grep.push(format!("--include={}", quote(name)));
        }
        if self.context > 0 {
            rg.push(format!("--context {}", self.context));
            grep.push(format!("-C {}", self.context));
        }
        for directory in VCS_DIRECTORIES {
            grep.push(format!("--exclude-dir={directory}"));
            // After the model's glob: in ripgrep a later glob overrides an earlier one.
            rg.push(format!("--glob '!{directory}'"));
        }
        rg.push(format!("-- {pattern} {path}"));
        grep.push(format!("-e {pattern} -- {path}"));
        format!(
            "if command -v rg >/dev/null 2>&1; then echo {ENGINE_MARKER}rg >&2; {}; \
             else echo {ENGINE_MARKER}grep >&2; {}; fi | head -n {} | {}",
            rg.join(" "),
            grep.join(" "),
            lines + 1,
            cut_command()
        )
    }
}

#[async_trait]
impl Tool for GrepTool {
    search_tool_basics!(super::GREP);

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: super::GREP.to_string(),
            description: format!(
                "Search file contents for a pattern; returns matching lines as path:line:text. Searches the working \
                 directory unless path names a directory or file. The pattern is a regular expression unless literal \
                 is true; respects .gitignore when ripgrep is installed. Shows at most {GREP_LIMIT} matches by default \
                 and {MAX_LINES} lines or {}KB; lines over {MAX_LINE_BYTES} bytes are cut. Read-only.",
                MAX_BYTES / 1024
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regular expression, or text when literal is true" },
                    "path": { "type": "string", "description": "Directory or file to search (default: working directory)" },
                    "glob": { "type": "string", "description": "Only files matching this glob, such as '*.rs' or 'src/**/*.ts'" },
                    "ignoreCase": { "type": "boolean", "description": "Match regardless of case (default: false)" },
                    "literal": { "type": "boolean", "description": "Treat the pattern as plain text (default: false)" },
                    "context": { "type": "integer", "minimum": 0, "maximum": MAX_CONTEXT, "description": "Lines to show before and after each match (default: 0)" },
                    "limit": { "type": "integer", "minimum": 1, "description": format!("Most matches to show (default: {GREP_LIMIT})") }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        }
    }

    fn validate_arguments(&self, args: &Value) -> Result<(), String> {
        GrepArgs::parse(args).map(|_| ())
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, String> {
        let args = GrepArgs::parse(&args)?;
        let limit = positive(args.limit, GREP_LIMIT, "limit")?;
        // A match with its context, and the separator between groups of them.
        let lines = limit.saturating_mul(2 * args.context + 2).min(MAX_LINES);
        let searched = self.settings.run(&args.script(lines), ctx).await?;
        // Without context every line is a match, so only `limit` of them are shown.
        let shown = if args.context == 0 { limit } else { lines };
        finish(
            &searched,
            shown,
            Wording {
                empty: "No matches found.",
                more: format!(
                    "more matches exist; showing about {limit}. Narrow the pattern, path or glob, or raise limit"
                ),
                cut: Some("long lines or output were cut"),
            },
        )
    }
}

// ---------------------------------------------------------------------------------------
// find
// ---------------------------------------------------------------------------------------

/// Paths shown by default (pi: find's `DEFAULT_LIMIT`).
const FIND_LIMIT: usize = 1000;

/// Finds files whose paths match a glob.
#[derive(Default)]
pub struct FindTool {
    pub settings: SearchSettings,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FindArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

impl FindArgs {
    fn parse(args: &Value) -> Result<Self, String> {
        let args: Self = parse_args(args)?;
        if args.pattern.trim().is_empty() {
            return Err("pattern is empty".into());
        }
        positive(args.limit, FIND_LIMIT, "limit")?;
        Ok(args)
    }

    /// The shell script, run in the search directory: `rg --files` when installed, otherwise
    /// `find`, sorted, at most `limit + 1` paths.
    fn script(&self, limit: usize) -> String {
        let directory = quote(&search_path(self.path.as_deref()));
        let pattern = self.pattern.trim();
        // find matches a pattern without a slash against the name; one with a slash against
        // the path from the search directory, where `*` also matches `/`, so `**/` can go.
        let test = if pattern.contains('/') {
            let path = pattern.trim_start_matches("./").replace("**/", "");
            format!("-path {}", quote(&format!("./{path}")))
        } else {
            format!("-name {}", quote(pattern))
        };
        // After the model's glob: in ripgrep a later glob overrides an earlier one.
        let excluded = VCS_DIRECTORIES
            .iter()
            .map(|directory| format!("--glob '!{directory}'"))
            .collect::<Vec<_>>()
            .join(" ");
        let prune = VCS_DIRECTORIES
            .iter()
            .map(|directory| format!("-name {directory}"))
            .collect::<Vec<_>>()
            .join(" -o ");
        format!(
            "cd -- {directory} || exit 2; \
             if command -v rg >/dev/null 2>&1; then echo {ENGINE_MARKER}rg >&2; \
             rg --no-config --files --hidden --color=never --glob {} {excluded}; \
             else echo {ENGINE_MARKER}find >&2; \
             find . \\( {prune} \\) -prune -o {test} -type f -print | sed 's|^\\./||'; \
             fi | LC_ALL=C sort | head -n {} | {}",
            quote(pattern),
            limit + 1,
            cut_command()
        )
    }
}

#[async_trait]
impl Tool for FindTool {
    search_tool_basics!(super::FIND);

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: super::FIND.to_string(),
            description: format!(
                "Find files by glob, such as '*.rs', '**/*.json' or 'src/**/*.spec.ts'; returns paths relative to the \
                 searched directory, sorted. A pattern without '/' matches file names at any depth. Respects .gitignore \
                 when ripgrep is installed. Shows at most {FIND_LIMIT} paths by default. Read-only."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob for the file paths" },
                    "path": { "type": "string", "description": "Directory to search (default: working directory)" },
                    "limit": { "type": "integer", "minimum": 1, "description": format!("Most paths to show (default: {FIND_LIMIT})") }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        }
    }

    fn validate_arguments(&self, args: &Value) -> Result<(), String> {
        FindArgs::parse(args).map(|_| ())
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, String> {
        let args = FindArgs::parse(&args)?;
        let limit = positive(args.limit, FIND_LIMIT, "limit")?;
        let searched = self.settings.run(&args.script(limit), ctx).await?;
        finish(
            &searched,
            limit,
            Wording {
                empty: "No files found.",
                more: format!(
                    "more files match; showing the first {limit}. Narrow the pattern or path, or raise limit"
                ),
                cut: Some("long paths or output were cut"),
            },
        )
    }
}

// ---------------------------------------------------------------------------------------
// ls
// ---------------------------------------------------------------------------------------

/// Entries shown by default (pi: ls's `DEFAULT_LIMIT`).
const LS_LIMIT: usize = 500;

/// Lists a directory.
#[derive(Default)]
pub struct LsTool {
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
    search_tool_basics!(super::LS);

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: super::LS.to_string(),
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
        let script = format!("LC_ALL=C ls -1Ap -- {path} | head -n {} | {}", limit + 1, cut_command());
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
    use std::path::{Path, PathBuf};

    use tokio_util::sync::CancellationToken;

    use crate::executor::{Captured, CommandOutput};

    /// An executor whose searches find one line and print `stderr` as given.
    struct StderrBytes(Vec<u8>);

    #[async_trait]
    impl Executor for StderrBytes {
        async fn run(&self, _: &ShellCommand<'_>, _: &CancellationToken) -> Result<CommandOutput, String> {
            Ok(CommandOutput {
                stdout: Captured {
                    bytes: b"src/a.rs:1:hit\n".to_vec(),
                    dropped: 0,
                },
                stderr: Captured {
                    bytes: self.0.clone(),
                    dropped: 0,
                },
                end: CommandEnd::Exited(0),
            })
        }

        async fn read_file(&self, _: &Path, _: u64) -> Result<Vec<u8>, String> {
            Err("not a file executor".into())
        }
    }

    #[tokio::test]
    async fn the_engine_marker_is_read_from_error_output_that_is_not_utf8() {
        let tool = GrepTool {
            settings: SearchSettings {
                executor: Arc::new(StderrBytes(b"zx-engine=grep\n\xff".to_vec())),
                ..SearchSettings::default()
            },
        };
        let ctx = ToolContext::new(PathBuf::from("."), CancellationToken::new());
        let output = tool.execute(json!({"pattern": "hit"}), &ctx).await.unwrap();
        assert_eq!(output.details.unwrap()["engine"], "grep", "{}", output.content);
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
        assert!(second.ends_with(" [line cut]"), "{second}");
        assert!(second.len() <= MAX_LINE_BYTES + " [line cut]".len());
        assert_eq!(bound("a\nb\n"), ("a\nb\n".to_string(), false));
    }

    #[test]
    fn find_turns_a_glob_with_a_slash_into_a_path_test() {
        let script = |pattern: &str| {
            FindArgs {
                pattern: pattern.into(),
                path: None,
                limit: None,
            }
            .script(10)
        };
        assert!(script("*.rs").contains("-name '*.rs'"));
        assert!(script("src/**/*.ts").contains("-path './src/*.ts'"));
        assert!(script("**/*.json").contains("-path './*.json'"));
        assert!(script("./a/b.txt").contains("-path './a/b.txt'"));
    }

    #[test]
    fn version_control_is_excluded_after_the_models_glob() {
        let grep = GrepArgs::parse(&json!({"pattern": "x", "glob": "*"}))
            .unwrap()
            .script(10);
        let find = FindArgs::parse(&json!({"pattern": "*"})).unwrap().script(10);
        for script in [grep, find] {
            let model = script.find("--glob '*'").unwrap();
            let excluded = script.find("--glob '!.git'").unwrap();
            assert!(model < excluded, "{script}");
        }
    }

    #[test]
    fn limits_are_capped_at_what_a_result_can_show() {
        assert_eq!(positive(Some(1_000_000), 10, "limit"), Ok(MAX_LINES));
        assert_eq!(positive(None, 10, "limit"), Ok(10));
    }

    #[test]
    fn limits_and_context_are_checked() {
        assert!(GrepArgs::parse(&json!({"pattern": "x", "limit": 0})).is_err());
        assert!(GrepArgs::parse(&json!({"pattern": "x", "context": MAX_CONTEXT + 1})).is_err());
        assert!(GrepArgs::parse(&json!({"pattern": ""})).is_err());
        assert!(FindArgs::parse(&json!({"pattern": " "})).is_err());
        assert!(LsArgs::parse(&json!({"limit": 0})).is_err());
        assert!(LsArgs::parse(&json!({"path": "x", "extra": 1})).is_err());
        assert!(GrepArgs::parse(&json!({"pattern": "x", "ignoreCase": true, "context": 2})).is_ok());
    }
}
