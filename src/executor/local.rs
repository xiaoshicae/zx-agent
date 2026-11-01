//! Commands and files on this machine.

use std::path::Path;
use std::process::Stdio;

use async_trait::async_trait;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use super::capture::{finish_reader, spawn_reader, stop_child, take_capture};
use super::{CommandEnd, CommandOutput, Executor, ShellCommand};

/// Runs commands as child processes of the agent and reads files directly.
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalExecutor;

/// Owns the process group until it has been signalled, including future-drop paths.
struct ProcessGroup(Option<u32>);

impl ProcessGroup {
    fn kill(&mut self) {
        let pid = self.0.take();
        #[cfg(unix)]
        if let Some(pid) = pid.and_then(|pid| libc::pid_t::try_from(pid).ok()) {
            // SAFETY: kill(2) has no memory-safety preconditions. The child leads this
            // process group, so a negative PID targets its pipelines and descendants.
            // ESRCH only means the group has already ended, which is the goal.
            unsafe {
                // Freeze the group first: dying pipeline children could otherwise wake
                // the shell before its SIGKILL arrives, letting it run the next command.
                libc::kill(-pid, libc::SIGSTOP);
                libc::kill(-pid, libc::SIGKILL);
            }
        }
        // Other platforms still terminate the child through kill_on_drop/start_kill.
        #[cfg(not(unix))]
        let _ = pid;
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

#[async_trait]
impl Executor for LocalExecutor {
    async fn run(&self, request: &ShellCommand<'_>, cancel: &CancellationToken) -> Result<CommandOutput, String> {
        let mut command = Command::new(request.shell);
        command
            .arg("-c")
            .arg(request.command)
            .current_dir(request.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for name in request.hidden_env {
            command.env_remove(name);
        }
        // Lead a new process group so a timeout can stop pipelines and background children.
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .spawn()
            .map_err(|e| format!("failed to start {}: {e}", request.shell))?;
        let mut group = ProcessGroup(child.id());
        let (stdout, stdout_reader) = spawn_reader(child.stdout.take());
        let (stderr, stderr_reader) = spawn_reader(child.stderr.take());

        let end = tokio::select! {
            status = child.wait() => match status {
                Ok(status) => match status.code() {
                    Some(code) => CommandEnd::Exited(code),
                    None => CommandEnd::Signaled,
                },
                Err(e) => CommandEnd::WaitFailed(e.to_string()),
            },
            _ = tokio::time::sleep(request.timeout) => CommandEnd::TimedOut,
            _ = cancel.cancelled() => CommandEnd::Cancelled,
        };
        // Kill the group after a normal exit too: background children (`cmd &`, daemons) would
        // otherwise outlive the call and the agent. It also closes the output pipes they hold,
        // so reading does not wait for PIPE_DRAIN_TIMEOUT.
        group.kill();
        stop_child(&mut child).await;
        for reader in [stdout_reader, stderr_reader].into_iter().flatten() {
            finish_reader(reader).await;
        }
        Ok(CommandOutput {
            stdout: take_capture(&stdout),
            stderr: take_capture(&stderr),
            end,
        })
    }

    async fn read_file(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, String> {
        let mut options = tokio::fs::OpenOptions::new();
        options.read(true);
        // A path can be replaced with a FIFO between lookup and open. Nonblocking open
        // lets us inspect the opened descriptor without waiting for a pipe writer.
        #[cfg(unix)]
        options.custom_flags(libc::O_NONBLOCK);
        let file = options
            .open(path)
            .await
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let meta = file
            .metadata()
            .await
            .map_err(|e| format!("cannot inspect {}: {e}", path.display()))?;
        if meta.is_dir() {
            return Err(format!("{} is a directory; use bash `ls` to list it", path.display()));
        }
        if !meta.is_file() {
            return Err(format!(
                "{} is not a regular file; special files are not supported",
                path.display()
            ));
        }
        if meta.len() > max_bytes {
            return Err(format!(
                "{} is {} bytes, over the {} byte limit; use bash with head, tail, or grep instead",
                path.display(),
                meta.len(),
                max_bytes
            ));
        }
        // Metadata can be stale (growing files and procfs). Bound the actual read too,
        // using one extra byte to distinguish an exact-limit file from an oversized one.
        let mut bytes = Vec::new();
        file.take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .await
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let read_bytes = u64::try_from(bytes.len()).map_err(|e| format!("file length is out of range: {e}"))?;
        if read_bytes > max_bytes {
            return Err(format!(
                "{} grew over the {} byte limit; use bash with head, tail, or grep instead",
                path.display(),
                max_bytes
            ));
        }
        Ok(bytes)
    }
}
