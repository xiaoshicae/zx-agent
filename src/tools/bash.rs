//! `bash` tool: run a shell command with a timeout and bounded output.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::truncate::{MAX_BYTES, MAX_LINES, truncate_tail};
use crate::agent::{Effects, Tool, ToolContext, ToolOutput};
use crate::ai::ToolDef;
use crate::executor::{CommandEnd, Executor, LocalExecutor, ShellCommand};

/// Runs `<shell> -c <command>` in the working directory.
pub struct BashTool {
    /// Shell program, looked up on `PATH`.
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

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        super::BASH
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
        let args = Args::deserialize(args).map_err(|e| format!("invalid arguments: {e}"))?;
        if args.command.trim().is_empty() {
            return Err("command is empty".to_string());
        }
        Ok(())
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, String> {
        let args: Args = serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))?;
        if args.command.trim().is_empty() {
            return Err("command is empty".to_string());
        }
        let limit = args
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(self.default_timeout)
            .min(self.max_timeout);

        let request = ShellCommand {
            shell: &self.shell,
            command: &args.command,
            cwd: &ctx.cwd,
            timeout: limit,
            hidden_env: &self.hidden_env,
        };
        let output = self.executor.run(&request, &ctx.cancel).await?;
        let (stdout, stderr) = (output.stdout, output.stderr);
        let mut text = String::from_utf8_lossy(&stdout.bytes).into_owned();
        let stderr_text = String::from_utf8_lossy(&stderr.bytes);
        if !stderr_text.trim().is_empty() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str("[stderr]\n");
            text.push_str(&stderr_text);
        }

        let truncated = truncate_tail(&text, MAX_LINES, MAX_BYTES);
        let mut content = String::new();
        if truncated.truncated || stdout.dropped + stderr.dropped > 0 {
            content.push_str("[output truncated; showing the end]\n");
        }
        content.push_str(&truncated.text);
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }

        let (status, is_error) = match output.end {
            CommandEnd::Exited(0) => ("[exit code: 0]".to_string(), false),
            CommandEnd::Exited(code) => (format!("[exit code: {code}]"), true),
            CommandEnd::Signaled => ("[terminated by a signal]".to_string(), true),
            CommandEnd::WaitFailed(e) => (format!("[failed to wait for the process: {e}]"), true),
            CommandEnd::TimedOut => (
                format!("[timed out after {}s; process group killed]", limit.as_secs()),
                true,
            ),
            CommandEnd::Cancelled => ("[cancelled; process group killed]".to_string(), true),
        };
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
    async fn long_output_keeps_the_tail() {
        let tool = BashTool::default();
        let output = tool.execute(json!({"command": "seq 1 5000"}), &ctx()).await.unwrap();
        assert!(output.content.starts_with("[output truncated; showing the end]\n"));
        assert!(output.content.contains("\n5000\n[exit code: 0]"));
        assert!(!output.content.contains("\n1\n"));
    }
}
