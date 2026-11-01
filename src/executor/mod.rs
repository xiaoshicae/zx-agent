//! Where built-in tools run commands and read files.
//!
//! Tools keep their arguments, result format and truncation; an executor only runs a
//! command or reads a file. [`LocalExecutor`] uses this machine; [`ContainerExecutor`]
//! runs the same tools in an isolated Docker container.

mod capture;
mod container;
mod local;

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

pub use container::{ContainerConfig, ContainerExecutor, Platform};
pub use local::LocalExecutor;

/// One shell command: `<shell> -c <command>` in `cwd`.
pub struct ShellCommand<'a> {
    pub shell: &'a str,
    pub command: &'a str,
    pub cwd: &'a Path,
    pub timeout: Duration,
    /// Environment variables the command must not see, such as the API key variable.
    /// An executor whose commands never inherit the agent's environment has nothing to remove.
    pub hidden_env: &'a [String],
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

/// Runs commands and reads files for the built-in tools.
#[async_trait]
pub trait Executor: Send + Sync {
    /// Run `command` until it exits, its timeout passes, or `cancel` fires. Then, and when the
    /// future is dropped, the processes it started are stopped, background children included.
    /// How far that reaches depends on the executor: [`LocalExecutor`] stops the command's
    /// process group, so a child that leaves it (`setsid`, a daemon) can keep running;
    /// [`ContainerExecutor`] stops every process of the command user. `Err` means the command
    /// could not start; the text is for the model.
    async fn run(&self, command: &ShellCommand<'_>, cancel: &CancellationToken) -> Result<CommandOutput, String>;

    /// Read a regular file of at most `max_bytes`. Directories, special files and larger
    /// files are errors whose text tells the model what to do instead. Callers that must
    /// respond to cancellation race this future against their token.
    async fn read_file(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, String>;
}
