//! Commands and files on this machine.

use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::Stdio;

use async_trait::async_trait;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use super::capture::{Capture, await_end, collect};
use super::environment::machine_section;
use super::{CommandOutput, Executor, SHELL, ShellCommand, directory_error, over_limit_error, special_file_error};
use crate::files::{Links, Unreadable, read_regular};

/// Runs commands as child processes of the agent, with the `bash` found on `PATH`, and reads
/// and writes files directly.
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
    /// This machine: its operating system and version, the time and the working directory.
    fn environment_section(&self, cwd: &Path) -> String {
        machine_section(cwd)
    }

    async fn run(&self, request: &ShellCommand<'_>, cancel: &CancellationToken) -> Result<CommandOutput, String> {
        let mut command = Command::new(SHELL);
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
        let mut child = command.spawn().map_err(|e| format!("failed to start {SHELL}: {e}"))?;
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
        read_regular(path, max_bytes, Links::Follow)
            .await
            .map_err(|unreadable| match unreadable {
                Unreadable::Io(error) => format!("cannot read {}: {error}", path.display()),
                Unreadable::Directory => directory_error(path),
                // Links are followed, so none is refused as a link.
                Unreadable::Link | Unreadable::Special => special_file_error(path),
                Unreadable::Over => over_limit_error(path, max_bytes),
            })
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
        let (target, bytes) = (path.to_path_buf(), contents.to_vec());
        // Opening, truncating and writing are one step on a blocking thread, which nothing
        // interrupts: once it has begun it runs to its end, whether this future is dropped,
        // as a cancelled run drops its calls, or the runtime shuts down. Done step by step
        // from here, a drop between the steps would leave the file empty or cut short. It is
        // still written in place, so the file keeps its inode, mode and links.
        let written = tokio::task::spawn_blocking(move || overwrite_regular_file(&target, &bytes)).await;
        match written {
            Ok(result) => result,
            // The thread never ran the write, or panicked in it.
            Err(error) => Err(format!(
                "cannot write {display}: the write did not run to its end: {error}"
            )),
        }
    }
}

/// Replace the contents of the file at `path` with `bytes`, creating it when missing and
/// refusing what is not a regular file. Opened without blocking, like the reads of `files`:
/// opening a FIFO for writing waits for a reader. Truncated only once known to be a regular
/// file.
fn overwrite_regular_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let unwritable = |e: std::io::Error| format!("cannot write {}: {e}", path.display());
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK);
    let mut file = options.open(path).map_err(unwritable)?;
    regular_file(path, &file.metadata().map_err(unwritable)?)?;
    file.set_len(0).map_err(unwritable)?;
    file.write_all(bytes).map_err(unwritable)
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// Long enough for an open that does not wait; one that waits for the other end never
    /// ends by itself.
    const PROMPTLY: Duration = Duration::from_secs(5);

    #[cfg(unix)]
    #[tokio::test]
    async fn a_fifo_in_place_of_a_checked_file_is_not_written_and_not_waited_on() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fifo");
        crate::test_support::make_fifo(&path);
        let target = path.clone();

        let written = tokio::time::timeout(
            PROMPTLY,
            tokio::task::spawn_blocking(move || overwrite_regular_file(&target, b"x")),
        )
        .await;
        // Opening the read end ends an open of the write end that waits.
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .unwrap();

        let refused = written.expect("the write waited for a reader").unwrap();
        assert!(refused.is_err(), "{refused:?}");
    }

    #[tokio::test]
    async fn overwriting_a_file_replaces_its_contents_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "a longer original").unwrap();

        overwrite_regular_file(&path, b"short").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "short");
    }
}
