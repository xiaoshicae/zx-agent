//! `bash` tool: run a shell command with a timeout and bounded output.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::truncate::{MAX_BYTES, MAX_LINES, truncate_tail};
use crate::agent::{Effects, ExecutionMode, Tool, ToolContext, ToolOutput};
use crate::ai::ToolDef;
use crate::executor::{Captured, CommandEnd, Executor, LocalExecutor, ShellCommand};

/// Runs `<shell> -c <command>` in the working directory.
pub struct BashTool {
    /// Shell program, looked up on `PATH`. A [`ContainerExecutor`] runs commands with the
    /// shell of its own configuration instead.
    ///
    /// [`ContainerExecutor`]: crate::executor::ContainerExecutor
    pub shell: String,
    /// Timeout when the call does not set `timeout_secs`.
    pub default_timeout: Duration,
    /// Upper bound for `timeout_secs`.
    pub max_timeout: Duration,
    /// Environment variables removed before the command runs, such as the one holding the
    /// API key. Commands otherwise inherit the agent's whole environment, and `env` or
    /// `echo $NAME` would put a secret into the tool result.
    pub hidden_env: Vec<String>,
    /// Where commands run. Defaults to this machine.
    pub executor: Arc<dyn Executor>,
}

impl Default for BashTool {
    fn default() -> Self {
        Self {
            shell: "bash".to_string(),
            default_timeout: Duration::from_secs(120),
            max_timeout: Duration::from_secs(600),
            hidden_env: Vec::new(),
            executor: Arc::new(LocalExecutor),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    command: String,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

impl BashTool {
    /// The limit for one call: what it asks for, else the default, never above the maximum.
    fn timeout_for(&self, requested_secs: Option<u64>) -> Duration {
        requested_secs
            .map(Duration::from_secs)
            .unwrap_or(self.default_timeout)
            .min(self.max_timeout)
    }
}

/// Of the output budget, the share stderr keeps when both streams are long: the answer is
/// usually on stdout. Room one stream leaves unused goes to the other.
const STDERR_SHARE: usize = 4;

/// Split `budget` between stdout and stderr, which need `stdout` and `stderr` of it.
fn split(budget: usize, stdout: usize, stderr: usize) -> (usize, usize) {
    let for_stderr = stderr.min((budget / STDERR_SHARE).max(budget.saturating_sub(stdout)));
    (budget - for_stderr, for_stderr)
}

/// stdout, then stderr after a `[stderr]` marker, each cut from its start to fit its share
/// of the budget and noted when cut, so a long stderr never hides stdout or the marker.
fn streams(stdout: &Captured, stderr: &Captured) -> String {
    let out = String::from_utf8_lossy(&stdout.bytes);
    let err = String::from_utf8_lossy(&stderr.bytes);
    let mut text = String::new();
    if err.trim().is_empty() {
        let kept = truncate_tail(&out, MAX_LINES, MAX_BYTES);
        if kept.truncated || stdout.dropped + stderr.dropped > 0 {
            text.push_str("[output truncated; showing the end]\n");
        }
        text.push_str(&kept.text);
        return text;
    }
    let (out_bytes, err_bytes) = split(MAX_BYTES, out.len(), err.len());
    let (out_lines, err_lines) = split(MAX_LINES, out.lines().count(), err.lines().count());
    let kept_out = truncate_tail(&out, out_lines, out_bytes);
    let kept_err = truncate_tail(&err, err_lines, err_bytes);
    if kept_out.truncated || stdout.dropped > 0 {
        text.push_str("[output truncated; showing the end]\n");
    }
    text.push_str(&kept_out.text);
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("[stderr]\n");
    if kept_err.truncated || stderr.dropped > 0 {
        text.push_str("[stderr truncated; showing the end]\n");
    }
    text.push_str(&kept_err.text);
    text
}

/// The closing status line, and whether the call failed.
fn status(end: &CommandEnd, limit: Duration) -> (String, bool) {
    match end {
        CommandEnd::Exited(0) => ("[exit code: 0]".to_string(), false),
        CommandEnd::Exited(code) => (format!("[exit code: {code}]"), true),
        CommandEnd::Signaled => ("[terminated by a signal]".to_string(), true),
        CommandEnd::WaitFailed(e) => (format!("[failed to wait for the process: {e}]"), true),
        CommandEnd::TimedOut => (
            format!("[timed out after {}s; process group killed]", limit.as_secs()),
            true,
        ),
        CommandEnd::Cancelled => ("[cancelled; process group killed]".to_string(), true),
    }
}

/// The same checks run before policy and approval, and again for direct SDK calls.
fn parse(args: &Value) -> Result<Args, String> {
    let args = Args::deserialize(args).map_err(|e| format!("invalid arguments: {e}"))?;
    if args.command.trim().is_empty() {
        return Err("command is empty".to_string());
    }
    // A zero limit would end the command before it could run.
    if args.timeout_secs == Some(0) {
        return Err("timeout_secs must be at least 1; omit it to use the default timeout".to_string());
    }
    Ok(args)
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        super::BASH
    }

    /// The mode of the executor: calls run one at a time when it handles one at a time.
    fn execution_mode(&self) -> ExecutionMode {
        self.executor.execution_mode()
    }

    /// Commands can change anything the process can reach, so every call is a side effect.
    fn effects(&self) -> Effects {
        Effects::SideEffects
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: super::BASH.to_string(),
            description: format!(
                "Run a shell command in the working directory. Returns stdout, then stderr under a [stderr] marker, \
                 then the exit code. Output is truncated to the last {MAX_LINES} lines or {}KB. Commands time out \
                 after {}s unless timeout_secs is set (max {}s); avoid commands that never exit, such as `tail -f`. \
                 Background processes started by the command are killed when it finishes.",
                MAX_BYTES / 1024,
                self.default_timeout.as_secs(),
                self.max_timeout.as_secs()
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to run" },
                    "timeout_secs": { "type": "integer", "minimum": 1, "description": "Timeout in seconds" }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    fn validate_arguments(&self, args: &Value) -> Result<(), String> {
        parse(args).map(|_| ())
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, String> {
        let args = parse(&args)?;
        let limit = self.timeout_for(args.timeout_secs);
        let request = ShellCommand {
            shell: &self.shell,
            command: &args.command,
            cwd: &ctx.cwd,
            timeout: limit,
            hidden_env: &self.hidden_env,
        };
        let output = self.executor.run(&request, &ctx.cancel).await?;
        let mut content = streams(&output.stdout, &output.stderr);
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        let (status, is_error) = status(&output.end, limit);
        content.push_str(&status);
        Ok(ToolOutput {
            content,
            is_error,
            terminate: false,
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Instant;
    use tokio::process::Command;
    use tokio_util::sync::CancellationToken;

    fn ctx() -> ToolContext {
        ToolContext {
            cwd: std::env::temp_dir(),
            cancel: CancellationToken::new(),
        }
    }

    #[test]
    fn stderr_keeps_a_quarter_of_the_budget_unless_stdout_leaves_it_more() {
        // Short stdout: stderr may use whatever stdout does not need.
        assert_eq!(split(100, 10, 500), (10, 90));
        // Both long: stderr keeps a quarter.
        assert_eq!(split(100, 500, 500), (75, 25));
        // Short stderr: stdout gets the rest.
        assert_eq!(split(100, 500, 5), (95, 5));
    }

    #[tokio::test]
    async fn a_large_stderr_never_hides_the_answer_on_stdout() {
        let tool = BashTool::default();
        let command = "printf 'ANSWER-ON-STDOUT\\n'; head -c 60000 /dev/zero | tr '\\0' 'e' >&2";

        let output = tool.execute(json!({ "command": command }), &ctx()).await.unwrap();

        assert!(
            output.content.starts_with("ANSWER-ON-STDOUT\n"),
            "{}",
            &output.content[..80]
        );
        assert!(output.content.contains("\n[stderr]\n"));
        assert!(output.content.contains("[stderr truncated; showing the end]"));
        assert!(output.content.len() <= MAX_BYTES + 200, "{}", output.content.len());
    }

    #[tokio::test]
    async fn captures_output_stderr_and_exit_code() {
        let tool = BashTool::default();
        let ok = tool.execute(json!({"command": "echo hello"}), &ctx()).await.unwrap();
        assert_eq!(ok.content, "hello\n[exit code: 0]");
        assert!(!ok.is_error);

        let failed = tool
            .execute(json!({"command": "echo out; echo bad >&2; exit 3"}), &ctx())
            .await
            .unwrap();
        assert_eq!(failed.content, "out\n[stderr]\nbad\n[exit code: 3]");
        assert!(failed.is_error);
    }

    #[tokio::test]
    async fn timeout_kills_the_whole_process_group() {
        let tool = BashTool::default();
        let started = Instant::now();
        let output = tool
            .execute(
                json!({"command": "echo start; sleep 30 | cat; echo never", "timeout_secs": 1}),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "took {:?}",
            started.elapsed()
        );
        assert!(output.is_error);
        assert!(output.content.starts_with("start\n"), "{}", output.content);
        assert!(output.content.contains("timed out after 1s"), "{}", output.content);
        assert!(!output.content.contains("never"), "{}", output.content);
    }

    #[tokio::test]
    async fn timed_out_pipelines_cannot_resume_the_shell() {
        let tool = BashTool {
            default_timeout: Duration::from_millis(30),
            ..BashTool::default()
        };
        let context = ctx();
        // Exercise concurrent pipeline exits: killing children must not wake a live shell
        // long enough to execute the next command. Bound concurrency to avoid FD exhaustion.
        for _ in 0..12 {
            let outputs = futures::future::join_all((0..8).map(|_| {
                tool.execute(
                    json!({"command": "sleep 30 | cat; echo post-timeout-command-ran"}),
                    &context,
                )
            }))
            .await;
            for output in outputs {
                let output = output.unwrap();
                assert!(output.is_error, "{}", output.content);
                assert!(output.content.contains("timed out"), "{}", output.content);
                assert!(
                    !output.content.contains("post-timeout-command-ran"),
                    "{}",
                    output.content
                );
            }
        }
    }

    #[tokio::test]
    async fn background_processes_end_with_the_command() {
        let output = BashTool::default()
            .execute(json!({"command": "sleep 30 & echo $!"}), &ctx())
            .await
            .unwrap();
        let pid = output.content.lines().next().unwrap().trim().to_string();
        let alive = || {
            std::process::Command::new("ps")
                .args(["-p", &pid])
                .output()
                .unwrap()
                .status
                .success()
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        while alive() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!alive(), "background process {pid} outlived the command");
    }

    #[tokio::test]
    async fn cancellation_stops_the_command() {
        let tool = BashTool::default();
        let ctx = ctx();
        let cancel = ctx.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            cancel.cancel();
        });
        let output = tool.execute(json!({"command": "sleep 30"}), &ctx).await.unwrap();
        assert!(output.content.contains("cancelled"), "{}", output.content);
    }

    #[tokio::test]
    async fn dropping_a_command_stops_its_background_children() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("child.pid");
        let quoted = pid_path.to_string_lossy().replace('\'', "'\\''");
        let command = format!("sleep 30 & echo $! > '{quoted}'; wait");
        let running =
            tokio::spawn(async move { BashTool::default().execute(json!({"command": command}), &ctx()).await });
        // Wait for the child's published PID rather than assuming that spawn has finished.
        let pid = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(text) = tokio::fs::read_to_string(&pid_path).await
                    && let Ok(pid) = text.trim().parse::<u32>()
                {
                    break pid.to_string();
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        running.abort();
        assert!(running.await.unwrap_err().is_cancelled());
        let stopped = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let alive = Command::new("ps")
                    .args(["-p", &pid])
                    .output()
                    .await
                    .unwrap()
                    .status
                    .success();
                if !alive {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        if stopped.is_err() {
            // Cleanup also runs against the old implementation, before asserting failure.
            Command::new("kill").arg(&pid).status().await.unwrap();
        }
        assert!(
            stopped.is_ok(),
            "background child {pid} outlived the dropped tool future"
        );
    }

    #[tokio::test]
    async fn hidden_variables_are_removed_from_the_environment() {
        // Cargo sets this variable for every test process, so the shell inherits it unless hidden.
        let probe = json!({"command": "echo \"[${CARGO_MANIFEST_DIR+visible}]\""});
        let visible = BashTool::default().execute(probe.clone(), &ctx()).await.unwrap();
        assert!(visible.content.starts_with("[visible]\n"), "{}", visible.content);

        let tool = BashTool {
            hidden_env: vec!["CARGO_MANIFEST_DIR".to_string()],
            ..BashTool::default()
        };
        let hidden = tool.execute(probe, &ctx()).await.unwrap();
        assert!(hidden.content.starts_with("[]\n"), "{}", hidden.content);
    }

    #[tokio::test]
    async fn requested_timeouts_are_capped_by_max_timeout() {
        let tool = BashTool {
            max_timeout: Duration::from_secs(1),
            ..BashTool::default()
        };
        let output = tool
            .execute(json!({"command": "sleep 5", "timeout_secs": 100}), &ctx())
            .await
            .unwrap();
        assert!(output.content.contains("timed out after 1s"), "{}", output.content);
    }

    #[tokio::test]
    async fn reports_a_shell_killed_by_a_signal() {
        let output = BashTool::default()
            .execute(json!({"command": "kill -9 $$"}), &ctx())
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(
            output.content.ends_with("[terminated by a signal]"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn rejects_bad_arguments() {
        let tool = BashTool::default();
        assert!(tool.execute(json!({"cmd": "ls"}), &ctx()).await.is_err());
        assert!(tool.execute(json!({"command": "  "}), &ctx()).await.is_err());
    }

    #[tokio::test]
    async fn a_zero_timeout_is_rejected_before_the_command_runs() {
        let tool = BashTool::default();
        let args = json!({"command": "echo hi", "timeout_secs": 0});
        let error = tool.validate_arguments(&args).unwrap_err();
        assert!(error.contains("timeout_secs"), "{error}");
        assert!(tool.execute(args, &ctx()).await.is_err());
    }

    #[tokio::test]
    async fn long_output_keeps_the_tail() {
        let tool = BashTool::default();
        let output = tool.execute(json!({"command": "seq 1 5000"}), &ctx()).await.unwrap();
        assert!(output.content.starts_with("[output truncated; showing the end]\n"));
        assert!(output.content.contains("\n5000\n[exit code: 0]"));
        assert!(!output.content.contains("\n1\n"));
    }
}
