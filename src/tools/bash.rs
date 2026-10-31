//! `bash` tool: run a shell command with a timeout and bounded output.

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

use super::truncate::{MAX_BYTES, MAX_LINES, truncate_tail};
use crate::agent::{Tool, ToolContext, ToolOutput};
use crate::ai::ToolDef;
use crate::sync::lock;

/// Bytes kept per stream while reading. Older output is dropped; only the tail is returned.
const CAPTURE_LIMIT: usize = 1024 * 1024;
/// How long to wait for output pipes after the process ends. Background children that
/// inherited the pipes could otherwise keep them open forever.
const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

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
}

impl Default for BashTool {
    fn default() -> Self {
        Self {
            shell: "bash".to_string(),
            default_timeout: Duration::from_secs(120),
            max_timeout: Duration::from_secs(600),
            hidden_env: Vec::new(),
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

#[derive(Default)]
struct Capture {
    bytes: Vec<u8>,
    dropped: usize,
}

enum End {
    Exited(std::process::ExitStatus),
    WaitFailed(String),
    TimedOut,
    Cancelled,
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "bash".to_string(),
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

        let mut command = Command::new(&self.shell);
        command
            .arg("-c")
            .arg(&args.command)
            .current_dir(&ctx.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for name in &self.hidden_env {
            command.env_remove(name);
        }
        // Lead a new process group so a timeout can stop pipelines and background children.
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .spawn()
            .map_err(|e| format!("failed to start {}: {e}", self.shell))?;
        let pid = child.id();
        let (stdout, stdout_reader) = spawn_reader(child.stdout.take());
        let (stderr, stderr_reader) = spawn_reader(child.stderr.take());

        let end = tokio::select! {
            status = child.wait() => match status {
                Ok(status) => End::Exited(status),
                Err(e) => End::WaitFailed(e.to_string()),
            },
            _ = tokio::time::sleep(limit) => End::TimedOut,
            _ = ctx.cancel.cancelled() => End::Cancelled,
        };
        // Kill the group after a normal exit too: background children (`cmd &`, daemons) would
        // otherwise outlive the call and the agent. It also closes the output pipes they hold,
        // so reading does not wait for PIPE_DRAIN_TIMEOUT.
        kill_process_group(pid, &mut child).await;
        for reader in [stdout_reader, stderr_reader].into_iter().flatten() {
            finish_reader(reader).await;
        }

        let stdout = take_capture(&stdout);
        let stderr = take_capture(&stderr);
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

        let (status, is_error) = match end {
            End::Exited(status) => match status.code() {
                Some(0) => ("[exit code: 0]".to_string(), false),
                Some(code) => (format!("[exit code: {code}]"), true),
                None => ("[terminated by a signal]".to_string(), true),
            },
            End::WaitFailed(e) => (format!("[failed to wait for the process: {e}]"), true),
            End::TimedOut => (
                format!("[timed out after {}s; process group killed]", limit.as_secs()),
                true,
            ),
            End::Cancelled => ("[cancelled; process group killed]".to_string(), true),
        };
        content.push_str(&status);
        Ok(ToolOutput {
            content,
            is_error,
            terminate: false,
        })
    }
}

fn spawn_reader<R>(reader: Option<R>) -> (Arc<Mutex<Capture>>, Option<JoinHandle<()>>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let capture = Arc::new(Mutex::new(Capture::default()));
    let Some(mut reader) = reader else {
        return (capture, None);
    };
    // Shared buffer: output read so far survives even if the reader task is aborted.
    let shared = capture.clone();
    let handle = tokio::spawn(async move {
        let mut chunk = [0u8; 8192];
        // A read error ends the capture early; the exit status still reports how the command ended.
        while let Ok(n) = reader.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            let mut capture = lock(&shared);
            capture.bytes.extend_from_slice(&chunk[..n]);
            if capture.bytes.len() > CAPTURE_LIMIT {
                let excess = capture.bytes.len() - CAPTURE_LIMIT;
                capture.bytes.drain(..excess);
                capture.dropped += excess;
            }
        }
    });
    (capture, Some(handle))
}

async fn finish_reader(mut handle: JoinHandle<()>) {
    if tokio::time::timeout(PIPE_DRAIN_TIMEOUT, &mut handle).await.is_err() {
        handle.abort();
    }
}

fn take_capture(capture: &Mutex<Capture>) -> Capture {
    std::mem::take(&mut *lock(capture))
}

async fn kill_process_group(pid: Option<u32>, child: &mut Child) {
    // Kernel process ids are `pid_t` values, so the conversion back cannot fail in practice.
    #[cfg(unix)]
    if let Some(pid) = pid.and_then(|pid| libc::pid_t::try_from(pid).ok()) {
        // SAFETY: kill(2) has no memory-safety preconditions. A negative pid signals the
        // whole process group, which the shell leads because of `process_group(0)`. The group
        // id outlives the reaped shell while any member lives, because POSIX does not reuse a
        // process group id before the group ends.
        // The result is not checked: the only expected failure is that the group already exited.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    // Only the Unix process-group kill needs the pid.
    #[cfg(not(unix))]
    let _ = pid;
    // Fails only if the child already exited, which is the goal.
    let _ = child.start_kill();
    // Reap the child so it does not linger as a zombie. If it takes longer than this,
    // `kill_on_drop` still cleans up when `child` is dropped.
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Instant;
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
        assert!(!output.content.contains("never"));
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
