//! Where built-in tools run commands and read and write files.
//!
//! Tools keep their arguments, result format and truncation; an executor only runs a
//! command, reads a file or writes one, and says where it does so. [`LocalExecutor`] uses
//! this machine; [`ContainerExecutor`] runs the same tools in an isolated Docker container.
//! Both run commands with bash.
//!
//! - `local`: commands and files on this machine
//! - `container`: commands and files in one Docker container
//! - `capture`: the bounded capture of a child's output, which both use
//! - `environment`: what a system prompt says about this machine, and the parts a container's
//!   description shares with it

mod capture;
mod container;
mod environment;
mod local;

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use zx_core::tools::ExecutionMode;

pub use container::{Access, ContainerConfig, ContainerExecutor, InvalidContainerConfig, Network, UnusableWorkspace};
pub use local::LocalExecutor;

/// One command line, run in `cwd` with the executor's shell: the model's command for the bash
/// tool, a POSIX script for the search tools.
pub struct ShellCommand<'a> {
    pub command: &'a str,
    pub cwd: &'a Path,
    pub timeout: Duration,
}

/// What was kept of one output stream: the most recent bytes, up to the executor's limit.
#[derive(Debug, Default)]
pub struct Captured {
    pub bytes: Vec<u8>,
    /// Older bytes discarded to stay within the limit.
    pub dropped: usize,
}

/// How a command ended.
#[derive(Debug)]
pub enum CommandEnd {
    Exited(i32),
    /// Ended by a signal, so there is no exit code.
    Signaled,
    WaitFailed(String),
    TimedOut,
    Cancelled,
}

/// A finished command. The processes it started have been stopped as far as the executor can
/// track them; see [`Executor::run`].
#[derive(Debug)]
pub struct CommandOutput {
    pub stdout: Captured,
    pub stderr: Captured,
    pub end: CommandEnd,
}

/// Runs commands, and reads and writes files, for the built-in tools.
#[async_trait]
pub trait Executor: Send + Sync {
    /// Whether calls may run at the same time.
    ///
    /// The built-in tools declare the mode of their executor. One that handles a single call
    /// at a time is `Sequential`, so that the loop does not start a call, and record it as
    /// started, only for it to wait for another; a run cancelled meanwhile then reports it as
    /// not executed. Defaults to `Parallel`; [`ContainerExecutor`] is `Sequential`. Must not
    /// panic, like [`Tool::execution_mode`].
    ///
    /// [`Tool::execution_mode`]: zx_core::tools::Tool::execution_mode
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    /// What the `environment` section of a system prompt says about where this executor runs
    /// the commands of a session that works in `cwd`, so that the model need not spend calls
    /// finding out. [`LocalExecutor`] names this machine's operating system and
    /// [`ContainerExecutor`] its container and how it is isolated, each with the time the
    /// section is made and the working directory. Defaults to the working directory alone.
    ///
    /// Called when a session is set up; reading a few small local files there is fine.
    fn environment_section(&self, cwd: &Path) -> String {
        format!("Working directory: {}", cwd.display())
    }

    /// Run `command` with the executor's shell until it exits, its timeout passes, or
    /// `cancel` fires. The built-in executors run `bash -c <command>`, so bash syntax works
    /// wherever commands run.
    ///
    /// Then, and when the future is dropped, the processes it started are stopped, background
    /// children included. How far that reaches depends on the executor: [`LocalExecutor`]
    /// stops the command's process group, so a child that leaves it (`setsid`, a daemon) can
    /// keep running; [`ContainerExecutor`] stops every process of the command user. `Err`
    /// means the command could not start; the text is for the model.
    async fn run(&self, command: &ShellCommand<'_>, cancel: &CancellationToken) -> Result<CommandOutput, String>;

    /// Read a regular file of at most `max_bytes`. Directories, special files and larger
    /// files are errors whose text tells the model what to do instead. Callers that must
    /// respond to cancellation race this future against their token.
    async fn read_file(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, String>;

    /// Replace the contents of the file at `path` with `contents`, creating the file and its
    /// missing parent directories. Directories and special files are refused. The write is
    /// not atomic: a reader may see it half done.
    ///
    /// Once the write has begun it must run to its end, also when the future is dropped, as
    /// the loop drops the calls of a cancelled run: a write stopped half way would leave the
    /// file empty or cut short. [`LocalExecutor`] and [`ContainerExecutor`] finish it on a
    /// thread or task of its own. So callers that must respond to cancellation check their
    /// token before calling, and a caller that is dropped cannot tell when the write ends.
    async fn write_file(&self, path: &Path, contents: &[u8]) -> Result<(), String>;
}

/// The shell both built-in executors run commands with: the bash tool is named after it, and
/// the search tools' scripts are POSIX, which it runs too.
const SHELL: &str = "bash";

/// The error for `path` being a directory where a file was expected, with what the model
/// can do instead.
fn directory_error(path: &Path) -> String {
    format!("{} is a directory; use bash `ls` to list it", path.display())
}

/// The error for `path` being neither a regular file nor a directory.
fn special_file_error(path: &Path) -> String {
    format!(
        "{} is not a regular file; special files are not supported",
        path.display()
    )
}

/// Check that the `bytes` read from `path` are within `max_bytes`; the error tells the model
/// what to use instead.
fn check_read_within(path: &Path, bytes: &[u8], max_bytes: u64) -> Result<(), String> {
    let read = u64::try_from(bytes.len()).map_err(|e| format!("file length is out of range: {e}"))?;
    if read > max_bytes {
        return Err(format!(
            "{} is over the {max_bytes} byte limit; use bash with head, tail, or grep instead",
            path.display()
        ));
    }
    Ok(())
}
