//! Commands and files on this machine.

use std::path::Path;
use std::process::Stdio;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use super::capture::{Capture, await_end, collect};
use super::{CommandOutput, Executor, ShellCommand, check_read_within, directory_error, special_file_error};

/// Runs commands as child processes of the agent and reads files directly.
///
/// Each command leads its own process group, which is killed when the command ends, times out
/// or is cancelled. A process that leaves the group, for example with `setsid` or by
/// daemonizing, is not tracked and can outlive the command: this machine offers no portable
/// way to find it. Use [`super::ContainerExecutor`] when that must not happen.
#[derive(Clone, Debug, Default)]
pub struct LocalExecutor {
    /// Environment variables removed before every command runs, such as the one holding the
    /// API key. Commands otherwise inherit the agent's whole environment, and `env` or
    /// `echo $NAME` would put a secret into the tool result.
    pub hidden_env: Vec<String>,
}

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
        for name in &self.hidden_env {
            command.env_remove(name);
        }
        // Lead a new process group so a timeout can stop pipelines and background children.
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .spawn()
            .map_err(|e| format!("failed to start {}: {e}", request.shell))?;
        let mut group = ProcessGroup(child.id());
        let stdout = Capture::spawn(child.stdout.take());
        let stderr = Capture::spawn(child.stderr.take());
        let end = await_end(&mut child, request.timeout, cancel).await;
        // Kill the group after a normal exit too: background children (`cmd &`) would otherwise
        // outlive the call and the agent. It also closes the output pipes they hold, so reading
        // does not wait for the drain timeout. Children that left the group are out of reach.
        group.kill();
        Ok(collect(&mut child, stdout, stderr, end).await)
    }

    async fn read_file(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, String> {
        // Opening a special file can act by itself: it releases a FIFO's writer, and some
        // devices start working when opened. So only regular files are opened.
        let meta = tokio::fs::metadata(path)
            .await
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        regular_file(path, &meta)?;
        if meta.len() > max_bytes {
            return Err(format!(
                "{} is {} bytes, over the {} byte limit; use bash with head, tail, or grep instead",
                path.display(),
                meta.len(),
                max_bytes
            ));
        }
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        // Metadata can be stale (growing files and procfs). Bound the actual read too,
        // using one extra byte to distinguish an exact-limit file from an oversized one.
        let mut bytes = Vec::new();
        file.take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .await
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        check_read_within(path, &bytes, max_bytes)?;
        Ok(bytes)
    }

    async fn write_file(&self, path: &Path, contents: &[u8]) -> Result<(), String> {
        let display = path.display();
        match tokio::fs::metadata(path).await {
            Ok(meta) => regular_file(path, &meta)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|e| format!("cannot create the directory {}: {e}", parent.display()))?;
                }
            }
            Err(error) => return Err(format!("cannot write {display}: {error}")),
        }
        let mut file = tokio::fs::File::create(path)
            .await
            .map_err(|e| format!("cannot write {display}: {e}"))?;
        file.write_all(contents)
            .await
            .map_err(|e| format!("cannot write {display}: {e}"))?;
        // tokio hands writes to a blocking thread; flushing waits for the last of them.
        file.flush().await.map_err(|e| format!("cannot write {display}: {e}"))
    }
}

/// Refuse what is not a regular file, with a hint for directories.
fn regular_file(path: &Path, meta: &std::fs::Metadata) -> Result<(), String> {
    if meta.is_dir() {
        return Err(directory_error(path));
    }
    if !meta.is_file() {
        return Err(special_file_error(path));
    }
    Ok(())
}
