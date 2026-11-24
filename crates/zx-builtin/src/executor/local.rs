//! Commands and files on this machine.

use std::fs::{File, Metadata};
use std::io::{ErrorKind, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::Stdio;

use async_trait::async_trait;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use super::capture::{Capture, await_end, collect};
use super::environment::machine_section;
use super::{
    CommandOutput, Executor, SHELL, ShellCommand, directory_read_error, directory_write_error, over_limit_error,
    special_file_error,
};
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
        // The PID of a child always fits in a pid_t; one that did not could not be signalled.
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
        let mut child = command.spawn().map_err(|e| format!("cannot start {SHELL}: {e}"))?;
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
                Unreadable::Directory => directory_read_error(path),
                // Links are followed, so none is refused as a link.
                Unreadable::Link | Unreadable::Special => special_file_error(path),
                Unreadable::Over => over_limit_error(path, max_bytes),
            })
    }

    async fn write_file(&self, path: &Path, contents: &[u8]) -> Result<(), String> {
        let display = path.display();
        match tokio::fs::metadata(path).await {
            Ok(meta) => check_regular_file(path, &meta)?,
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
        // The whole write is one step on a blocking thread, which nothing interrupts: once it
        // has begun it runs to its end, whether this future is dropped, as a cancelled run
        // drops its calls, or the runtime shuts down. Done step by step from here, a drop
        // between the steps of a write in place would leave the file empty or cut short.
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
/// refusing what is not a regular file.
///
/// The bytes go into a new file beside the old one, which is renamed into its place: no reader
/// and no crash finds the file empty or cut short, and a write that fails, such as on a full
/// disk, leaves it as it was ([`replace`]). Where the new file would not be the same file, the
/// file is written in place, as it is through a symbolic link: truncated, once known to be a
/// regular file, and written.
fn overwrite_regular_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let link = match std::fs::symlink_metadata(path) {
        Ok(found) => Some(found.file_type().is_symlink()),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(unwritable(path, &error)),
    };
    let file = match link {
        None => match replace(path, None, bytes)? {
            Replacement::Done => return Ok(()),
            Replacement::InPlace => open_regular_file(path)?.0,
        },
        Some(is_link) => {
            // Opened as for a write in place, so that a file that may not be written is
            // refused however it would be written.
            let (file, metadata) = open_regular_file(path)?;
            if !is_link && let Replacement::Done = replace(path, Some(&metadata), bytes)? {
                return Ok(());
            }
            file
        }
    };
    write_in_place(file, path, bytes)
}

/// The error of a write of `path` that failed with `error`.
fn unwritable(path: &Path, error: &std::io::Error) -> String {
    format!("cannot write {}: {error}", path.display())
}

/// The file at `path`, opened for writing without truncating it and created when missing,
/// and its metadata. Opened without blocking, like the reads of `files`: opening a FIFO for
/// writing waits for a reader. Fails for what is not a regular file.
fn open_regular_file(path: &Path) -> Result<(File, Metadata), String> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK);
    let file = options.open(path).map_err(|e| unwritable(path, &e))?;
    let metadata = file.metadata().map_err(|e| unwritable(path, &e))?;
    check_regular_file(path, &metadata)?;
    Ok((file, metadata))
}

/// Replace the contents of the regular `file`, opened from `path`, with `bytes` where it is.
fn write_in_place(mut file: File, path: &Path, bytes: &[u8]) -> Result<(), String> {
    file.set_len(0).map_err(|e| unwritable(path, &e))?;
    file.write_all(bytes).map_err(|e| unwritable(path, &e))
}

/// How [`replace`] went, when it did not fail.
enum Replacement {
    /// The new file took the place of the old one, or of none.
    Done,
    /// Nothing changed: the new file would not be the same file, so the file is to be written
    /// in place.
    InPlace,
}

/// Write `bytes` into a new file in the directory of `path`, to disk, and rename it over
/// `path`, where it takes the place of the `existing` file, with its permissions and group,
/// or of none. Nothing changes, for a write in place, when the new file would not be the same
/// file: `existing` has more than one name or another owner, the directory takes no new file,
/// or the group or the rename is refused. Fails, with nothing changed, when the new file
/// cannot be written whole.
#[cfg(unix)]
fn replace(path: &Path, existing: Option<&Metadata>, bytes: &[u8]) -> Result<Replacement, String> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid takes no arguments, always succeeds and touches no memory.
    let user = unsafe { libc::geteuid() };
    if existing.is_some_and(|existing| existing.nlink() != 1 || existing.uid() != user) {
        return Ok(Replacement::InPlace);
    }
    let Some(directory) = path.parent() else {
        return Ok(Replacement::InPlace);
    };
    // A short name of its own, so that it fits wherever the file's name does.
    let temporary = directory.join(format!(".zx-agent-write-{}.tmp", uuid::Uuid::new_v4()));
    let file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
    {
        Ok(file) => file,
        // The directory takes no new file, while the file itself may still be written.
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem
            ) =>
        {
            return Ok(Replacement::InPlace);
        }
        Err(error) => return Err(nothing_changed(path, &error)),
    };
    let replaced = fill_and_rename(file, &temporary, path, existing, bytes);
    if !matches!(replaced, Ok(Replacement::Done)) {
        // Best effort: the new file did not take the old one's place, and nothing refers to it.
        let _ = std::fs::remove_file(&temporary);
    }
    replaced
}

/// Where new files are not renamed into place: every file is written in place.
#[cfg(not(unix))]
fn replace(_path: &Path, _existing: Option<&Metadata>, _bytes: &[u8]) -> Result<Replacement, String> {
    Ok(Replacement::InPlace)
}

/// Give the new `file` at `temporary` the permissions and group of the `existing` one, write
/// `bytes` into it to disk, and rename it over `path`, as [`replace`] does.
#[cfg(unix)]
fn fill_and_rename(
    mut file: File,
    temporary: &Path,
    path: &Path,
    existing: Option<&Metadata>,
    bytes: &[u8],
) -> Result<Replacement, String> {
    if let Some(existing) = existing
        && !takes_permissions_and_group(&file, existing)
    {
        return Ok(Replacement::InPlace);
    }
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| nothing_changed(path, &error))?;
    Ok(match std::fs::rename(temporary, path) {
        Ok(()) => Replacement::Done,
        // Such as a file mounted on its own, which nothing can be renamed over.
        Err(_) => Replacement::InPlace,
    })
}

/// Whether the new `file` now has the permissions and group of the `existing` one. Its group
/// is the directory's or this user's; another one is kept where this user may set it.
#[cfg(unix)]
fn takes_permissions_and_group(file: &File, existing: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    // A refusal only means that the new file would differ: the file is written in place then.
    if file.set_permissions(existing.permissions()).is_err() {
        return false;
    }
    match file.metadata() {
        Ok(created) if created.gid() == existing.gid() => true,
        Ok(_) => std::os::unix::fs::fchown(file, None, Some(existing.gid())).is_ok(),
        Err(_) => false,
    }
}

/// The error of a replacement of `path` that failed with `error` before it changed anything.
#[cfg(unix)]
fn nothing_changed(path: &Path, error: &std::io::Error) -> String {
    format!("cannot write {}: {error}; the file was not changed", path.display())
}

/// Refuse to write what is not a regular file, with a hint for directories.
fn check_regular_file(path: &Path, meta: &std::fs::Metadata) -> Result<(), String> {
    if meta.is_dir() {
        return Err(directory_write_error(path));
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
    async fn overwriting_a_file_replaces_its_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "a longer original").unwrap();

        overwrite_regular_file(&path, b"short").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "short");
    }

    #[cfg(unix)]
    mod replacement {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        use std::path::Path;

        use super::super::overwrite_regular_file;

        fn read(path: &Path) -> String {
            std::fs::read_to_string(path).unwrap()
        }

        fn inode(path: &Path) -> u64 {
            std::fs::metadata(path).unwrap().ino()
        }

        fn set_mode(path: &Path, mode: u32) {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        }

        #[test]
        fn a_file_is_replaced_whole_by_a_new_one_with_its_permissions_and_group() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("f.txt");
            std::fs::write(&path, "old").unwrap();
            set_mode(&path, 0o640);
            let before = std::fs::metadata(&path).unwrap();

            overwrite_regular_file(&path, b"new").unwrap();

            let after = std::fs::metadata(&path).unwrap();
            assert_eq!(read(&path), "new");
            assert_ne!(after.ino(), before.ino(), "a new file took its place");
            assert_eq!(after.permissions().mode() & 0o7777, 0o640);
            assert_eq!(after.gid(), before.gid());
            // Nothing is left beside it.
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        }

        #[test]
        fn a_file_with_another_name_or_behind_a_link_is_written_in_place() {
            let dir = tempfile::tempdir().unwrap();
            let shared = dir.path().join("shared.txt");
            std::fs::write(&shared, "old").unwrap();
            let other_name = dir.path().join("other-name.txt");
            std::fs::hard_link(&shared, &other_name).unwrap();
            let target = dir.path().join("target.txt");
            std::fs::write(&target, "old").unwrap();
            let link = dir.path().join("link.txt");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            let (shared_inode, target_inode) = (inode(&shared), inode(&target));

            overwrite_regular_file(&other_name, b"through another name").unwrap();
            overwrite_regular_file(&link, b"through a link").unwrap();

            assert_eq!(read(&shared), "through another name");
            assert_eq!(inode(&shared), shared_inode);
            assert_eq!(read(&target), "through a link");
            assert_eq!(inode(&target), target_inode);
            assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        }

        #[test]
        fn a_new_file_is_created_whole_and_a_read_only_one_is_refused_unchanged() {
            let dir = tempfile::tempdir().unwrap();
            let new = dir.path().join("new.txt");
            let read_only = dir.path().join("read-only.txt");
            std::fs::write(&read_only, "kept").unwrap();
            set_mode(&read_only, 0o444);

            overwrite_regular_file(&new, b"fresh").unwrap();
            let refused = overwrite_regular_file(&read_only, b"x").unwrap_err();

            assert_eq!(read(&new), "fresh");
            assert!(refused.contains("Permission denied"), "{refused}");
            assert_eq!(read(&read_only), "kept");
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
        }

        #[test]
        fn a_file_in_a_directory_that_takes_no_new_file_is_written_in_place() {
            let dir = tempfile::tempdir().unwrap();
            let locked = dir.path().join("locked");
            std::fs::create_dir(&locked).unwrap();
            let path = locked.join("f.txt");
            std::fs::write(&path, "old").unwrap();
            let before = inode(&path);
            set_mode(&locked, 0o555);

            let written = overwrite_regular_file(&path, b"new");

            // Writable again, for the temporary directory to be removed.
            set_mode(&locked, 0o755);
            written.unwrap();
            assert_eq!(read(&path), "new");
            assert_eq!(inode(&path), before);
        }
    }
}
