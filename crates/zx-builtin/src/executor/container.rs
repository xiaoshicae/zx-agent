//! Commands and files inside one Docker container, isolated from this machine.
//!
//! The container is the sandbox, and inside it the agent may do what its user may: by
//! default the workspace and the container's own files are writable and the network is
//! Docker's default bridge. The walls stay: no privileged mode, no Docker socket, no host
//! network or PID namespace, no capabilities, none of the agent's environment. A stricter
//! sandbox, with a read-only workspace or root and no network, is a setting. Its PID 1
//! reads the stdin of the attached `docker run` client that the executor owns. Dropping the
//! executor kills the client, and if the agent dies the pipe closes; either way the
//! container is removed (`--rm`).
//!
//! Commands run as a non-root user, while PID 1 and its `cat` run as root without
//! capabilities. Around every command, a sweep run as the command user kills every process
//! that user owns (`kill -1`): children that left the process group with `setsid` are
//! stopped too, nothing the command can write decides what is stopped, and the root processes
//! that keep the container alive cannot be signalled. Because a sweep would also stop a
//! concurrent call, the executor runs one command, file read or file write at a time.
//!
//! - `config`: how the container is started and described: its settings and their checks
//!   (with what the start found about the workspace), its network, and the prompt section
//!   about it
//! - `scripts`: the shell scripts that run inside it, and the exit codes they answer with

mod config;
mod scripts;

use std::ffi::OsStr;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

pub use config::{Access, ContainerConfig, InvalidContainerConfig, Network, UnusableWorkspace};
use config::{Platform, check_workspace, container_section, parse_platform};
use scripts::{
    EXIT_DENIED, EXIT_DIRECTORY, EXIT_MISSING, EXIT_NO_DIRECTORY, EXIT_SPECIAL, PROBE, PROBE_MISSING,
    PROBE_NO_WORKSPACE, PROBE_READ_ONLY_WORKSPACE, PROBE_ROOT, READ, RUN, SWEEP, WRITE,
};

use super::capture::{Capture, await_end, collect};
use super::{
    Captured, CommandEnd, CommandOutput, Executor, SHELL, ShellCommand, directory_read_error, directory_write_error,
    over_limit_error, special_file_error,
};
use zx_core::tools::ExecutionMode;

/// Longest wait for a new container to accept commands.
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// Pause between readiness probes while the container starts.
const START_POLL: Duration = Duration::from_millis(100);
/// Longest wait for one readiness probe.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest wait for one sweep of the command user's processes.
const KILL_TIMEOUT: Duration = Duration::from_secs(10);
/// The user that PID 1 and its `cat` run as: root without capabilities, so that commands,
/// which must not run as root, cannot signal the processes that keep the container alive.
const KEEPER_USER: &str = "0:0";

/// Where `docker exec` runs calls: the client program, the container and the command user.
#[derive(Clone)]
struct Target {
    docker: PathBuf,
    name: String,
    user: String,
}

/// Whether a script reads its standard input.
#[derive(Clone, Copy)]
enum Input {
    Closed,
    /// Open for the caller to write: `docker exec --interactive`.
    Piped,
}

impl Target {
    /// `docker exec [--interactive] --user <user> [--workdir <dir>] <container> sh -c <script> <label> <arguments...>`.
    fn exec<I, S>(&self, workdir: Option<&Path>, input: Input, script: &str, label: &str, arguments: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = docker(&self.docker);
        command.arg("exec");
        if let Input::Piped = input {
            command.arg("--interactive");
        }
        command.args(["--user", &self.user]);
        if let Some(workdir) = workdir {
            command.arg("--workdir").arg(workdir);
        }
        command
            .arg(&self.name)
            .args(["sh", "-c", script, label])
            .args(arguments);
        if let Input::Piped = input {
            command.stdin(Stdio::piped());
        }
        command
    }

    /// Kill every process of the command user (see `SWEEP`). A failure leaves them running
    /// until the container is removed with the agent.
    async fn sweep(&self) {
        let mut command = self.exec(None, Input::Closed, SWEEP, "zx-agent-sweep", std::iter::empty::<&str>());
        command.stdout(Stdio::null()).stderr(Stdio::null());
        let _ = tokio::time::timeout(KILL_TIMEOUT, command.status()).await;
    }
}

/// Runs commands, and reads and writes files, in one container that lives as long as this
/// value. Commands run with the image's `bash`, which the start checks for.
pub struct ContainerExecutor {
    target: Target,
    /// What the container was started with, which its prompt section describes.
    config: ContainerConfig,
    /// Mounted at the same path.
    workspace: PathBuf,
    platform: Platform,
    /// Taken by one command, file read or file write at a time, because a sweep stops every
    /// process of the command user. Shared (`Arc`) with the tasks that outlive the call that
    /// started them and must wait for their turn like any other call: the cleanup of a
    /// dropped run, and a file write, which is finished once it has begun.
    turn: Arc<tokio::sync::Mutex<()>>,
    /// The attached `docker run` client: its open stdin keeps the container running, and
    /// killing it on drop removes the container.
    _client: Child,
    /// Drains the client's error output so that it never blocks on a full pipe.
    _client_stderr: Capture,
}

impl ContainerExecutor {
    /// Start a container with `config` that mounts `workspace` at the same path, and check
    /// that it can run commands. Fails, and removes the container, when the settings are
    /// invalid ([`ContainerConfig::validate`], whose error the result holds), the workspace is
    /// not an absolute path that Docker can mount, Docker cannot run, the image is not present
    /// locally, the container does not accept commands within 60 seconds, the image lacks a
    /// required program, commands would run as root, or the command user cannot use the
    /// workspace ([`UnusableWorkspace`], which the result holds too).
    ///
    /// Everything in the workspace is readable inside the container, and with
    /// [`Access::ReadWrite`] changeable, so keep files the container must not read or change,
    /// such as the session file, elsewhere.
    pub async fn start(config: ContainerConfig, workspace: &Path) -> Result<Self> {
        config.validate()?;
        check_workspace(workspace)?;
        let name = format!("zx-agent-{}", uuid::Uuid::new_v4().simple());
        let target = Target {
            docker: config.docker.clone(),
            name: name.clone(),
            user: config.user.clone(),
        };
        let mut command = docker(&config.docker);
        command
            .args(run_args(&config, workspace, &name))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut client = command
            .spawn()
            .with_context(|| format!("cannot run {}; is Docker installed?", config.docker.display()))?;
        let stderr = Capture::spawn(client.stderr.take());
        let started = Instant::now();
        let platform = loop {
            if let Some(status) = client.try_wait().context("cannot check the docker run client")? {
                bail!(
                    "docker run exited ({status}) before the container started: {}",
                    text(&stderr.finish().await.bytes)
                );
            }
            let reason = match probe(&target, &config, workspace).await {
                Probe::Ready(platform) => break platform,
                Probe::Unusable(reason) => bail!("container image {} cannot be used: {reason}", config.image),
                Probe::Workspace(problem) => return Err(problem.into()),
                Probe::NotReady(reason) => reason,
            };
            ensure!(
                started.elapsed() < START_TIMEOUT,
                "container {name} did not accept commands within {START_TIMEOUT:?}: {reason}"
            );
            tokio::time::sleep(START_POLL).await;
        };
        Ok(Self {
            target,
            config,
            workspace: workspace.to_path_buf(),
            platform,
            turn: Arc::new(tokio::sync::Mutex::new(())),
            _client: client,
            _client_stderr: stderr,
        })
    }

    /// The container's name, for example to find it with `docker ps`.
    pub fn name(&self) -> &str {
        &self.target.name
    }
}

/// A file operation: the words that name it in errors, and the longest wait for it.
struct FileOperation {
    /// Such as `read`.
    verb: &'static str,
    /// Such as `reading`.
    doing: &'static str,
    timeout: Duration,
}

/// Reading one file. A FIFO swapped in after the checks would otherwise block.
const READING: FileOperation = FileOperation {
    verb: "read",
    doing: "reading",
    timeout: Duration::from_secs(30),
};

/// Writing one file, with a time limit for the same reason.
const WRITING: FileOperation = FileOperation {
    verb: "write",
    doing: "writing",
    timeout: Duration::from_secs(30),
};

/// Run the file script `command` for `path`, within the time `operation` allows. `transfer`
/// takes the pipes it needs from the started script and moves the file's bytes through them;
/// then the script's exit status and error output are collected. Returns the status, what
/// `transfer` returned and the error output.
///
/// The caller holds the executor's turn meanwhile: a sweep for a command would also stop
/// this script.
async fn file_script<T, F>(
    mut command: Command,
    operation: &FileOperation,
    path: &Path,
    transfer: impl FnOnce(&mut Child) -> F,
) -> Result<(ExitStatus, T, Vec<u8>), String>
where
    F: Future<Output = Result<T, String>>,
{
    command.stderr(Stdio::piped());
    let script = async {
        let mut child = command.spawn().map_err(|e| format!("cannot start docker exec: {e}"))?;
        let stderr = Capture::spawn(child.stderr.take());
        let moved = transfer(&mut child).await?;
        let status = child
            .wait()
            .await
            .map_err(|e| format!("cannot {} {}: {e}", operation.verb, path.display()))?;
        Ok((status, moved, stderr.finish().await.bytes))
    };
    tokio::time::timeout(operation.timeout, script).await.map_err(|_| {
        format!(
            "{} {} in the container took longer than {:?}",
            operation.doing,
            path.display(),
            operation.timeout
        )
    })?
}

/// Write `contents` to `path` with the write script `command`, as
/// [`Executor::write_file`] describes for this executor. The caller holds the executor's
/// turn meanwhile.
async fn write_script(command: Command, path: &Path, contents: &[u8]) -> Result<(), String> {
    let write = |child: &mut Child| {
        let stdin = child.stdin.take();
        async move {
            if let Some(mut stdin) = stdin {
                // A script that refused the path exits without reading; its status explains why.
                if let Err(error) = stdin.write_all(contents).await
                    && error.kind() != std::io::ErrorKind::BrokenPipe
                {
                    return Err(format!("cannot write {}: {error}", path.display()));
                }
                // Dropping stdin closes it, which ends `cat`.
            }
            Ok(())
        }
    };
    let (status, (), stderr) = file_script(command, &WRITING, path, write).await?;
    match status.code() {
        Some(0) => Ok(()),
        Some(EXIT_DIRECTORY) => Err(directory_write_error(path)),
        Some(EXIT_SPECIAL) => Err(special_file_error(path)),
        Some(EXIT_NO_DIRECTORY) => Err(format!(
            "cannot create the directory of {} in the container: {}",
            path.display(),
            text(&stderr)
        )),
        _ => Err(format!(
            "cannot write {} in the container: {}",
            path.display(),
            text(&stderr)
        )),
    }
}

/// The result of a command that never started because its run was cancelled first.
fn not_started() -> CommandOutput {
    CommandOutput {
        stdout: Captured::default(),
        stderr: Captured::default(),
        end: CommandEnd::Cancelled,
    }
}

#[async_trait]
impl Executor for ContainerExecutor {
    /// One call at a time: see `turn`.
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    /// The container: its system, as it reported at start, the image, the working directory
    /// and whether commands may change it, the command user, the network and which of the
    /// container's own files are writable.
    fn environment_section(&self, cwd: &Path) -> String {
        container_section(&self.config, &self.workspace, &self.platform, cwd)
    }

    async fn run(&self, request: &ShellCommand<'_>, cancel: &CancellationToken) -> Result<CommandOutput, String> {
        // Declared before `cleanup`, so that a dropped run releases its turn only after the
        // cleanup task has been queued for the next one.
        let _turn = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(not_started()),
            turn = self.turn.lock() => turn,
        };
        // An earlier run that was dropped may have left processes whose sweep is still queued.
        self.target.sweep().await;
        // Commands never inherit the agent's environment, so there is nothing to hide from them.
        let mut command = self.target.exec(
            Some(request.cwd),
            Input::Closed,
            RUN,
            "zx-agent-run",
            [SHELL, request.command],
        );
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|e| format!("cannot start docker exec: {e}"))?;
        let cleanup = Cleanup {
            executor: self,
            armed: true,
        };
        let stdout = Capture::spawn(child.stdout.take());
        let stderr = Capture::spawn(child.stderr.take());
        let end = await_end(&mut child, request.timeout, cancel).await;
        // After a normal exit too: background children end with their command, including
        // ones that started their own session.
        cleanup.finish().await;
        Ok(collect(&mut child, stdout, stderr, end).await)
    }

    async fn read_file(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, String> {
        let limit = max_bytes.saturating_add(1);
        let limit_text = limit.to_string();
        let mut command = self.target.exec(
            None,
            Input::Closed,
            READ,
            "zx-agent-read",
            [path.as_os_str(), OsStr::new(&limit_text)],
        );
        command.stdout(Stdio::piped());
        let read = |child: &mut Child| {
            let stdout = child.stdout.take();
            async move {
                let mut bytes = Vec::new();
                if let Some(stdout) = stdout {
                    stdout
                        .take(limit)
                        .read_to_end(&mut bytes)
                        .await
                        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
                }
                Ok(bytes)
            }
        };
        let (status, bytes, stderr) = {
            let _turn = self.turn.lock().await;
            file_script(command, &READING, path, read).await?
        };
        match status.code() {
            Some(0) => {
                check_read_within(path, &bytes, max_bytes)?;
                Ok(bytes)
            }
            Some(EXIT_DIRECTORY) => Err(directory_read_error(path)),
            Some(EXIT_MISSING) => Err(format!("cannot read {}: no such file in the container", path.display())),
            Some(EXIT_SPECIAL) => Err(special_file_error(path)),
            Some(EXIT_DENIED) => Err(format!(
                "cannot read {}: permission denied in the container",
                path.display()
            )),
            _ => Err(format!(
                "cannot read {} in the container: {}",
                path.display(),
                text(&stderr)
            )),
        }
    }

    async fn write_file(&self, path: &Path, contents: &[u8]) -> Result<(), String> {
        let mut command = self
            .target
            .exec(None, Input::Piped, WRITE, "zx-agent-write", [path.as_os_str()]);
        command.stdout(Stdio::null());
        let turn = Arc::clone(&self.turn);
        let (target, bytes) = (path.to_path_buf(), contents.to_vec());
        // Awaited here, but not aborted when this future is dropped, as a cancelled run
        // drops its calls: the script truncates the file before it writes, and stopping it
        // in between would leave the file empty or cut short. So the write runs to its end
        // on its own, on the executor's turn like any call, within the time a write may take.
        let writing = tokio::spawn(async move {
            let _turn = turn.lock_owned().await;
            write_script(command, &target, &bytes).await
        });
        match writing.await {
            Ok(written) => written,
            // The task panicked, or the runtime shut down before it ended.
            Err(error) => Err(format!(
                "cannot write {} in the container: the write did not run to its end: {error}",
                path.display()
            )),
        }
    }
}

/// Sweeps the command user's processes when a command ends, including when the run future
/// is dropped before it does.
struct Cleanup<'a> {
    executor: &'a ContainerExecutor,
    armed: bool,
}

impl Cleanup<'_> {
    async fn finish(mut self) {
        self.armed = false;
        self.executor.target.sweep().await;
    }
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        // Spawning a Tokio task needs a runtime; without one the container's removal with
        // the agent is the only cleanup, and the next command's own sweep comes first anyway.
        if !self.armed || tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let target = self.executor.target.clone();
        let turn = Arc::clone(&self.executor.turn);
        // The sweep must outlive the dropped run, so it is not awaited. It takes its turn like
        // a call: the dropped run releases the lock right after this guard, and the sweep can
        // then never stop a command that started later.
        tokio::spawn(async move {
            let _turn = turn.lock_owned().await;
            target.sweep().await;
        });
    }
}

enum Probe {
    Ready(Platform),
    /// The image cannot be used; retrying will not help.
    Unusable(String),
    /// The command user cannot use the workspace; retrying will not help either.
    Workspace(UnusableWorkspace),
    /// The container is not accepting commands yet.
    NotReady(String),
}

async fn probe(target: &Target, config: &ContainerConfig, workspace: &Path) -> Probe {
    let mut command = target.exec(
        None,
        Input::Closed,
        PROBE,
        "zx-agent-probe",
        [
            OsStr::new(SHELL),
            workspace.as_os_str(),
            OsStr::new(match config.workspace_access {
                Access::ReadWrite => "rw",
                Access::ReadOnly => "ro",
            }),
        ],
    );
    let output = match tokio::time::timeout(PROBE_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => return Probe::NotReady(format!("cannot start docker exec: {e}")),
        Err(_) => {
            return Probe::NotReady(format!("docker exec did not answer within {PROBE_TIMEOUT:?}"));
        }
    };
    match output.status.code() {
        Some(0) => Probe::Ready(parse_platform(&String::from_utf8_lossy(&output.stdout))),
        Some(PROBE_ROOT | PROBE_MISSING) => Probe::Unusable(text(&output.stderr)),
        Some(PROBE_NO_WORKSPACE) => Probe::Workspace(UnusableWorkspace::Unreadable {
            user: config.user.clone(),
            workspace: workspace.to_path_buf(),
        }),
        Some(PROBE_READ_ONLY_WORKSPACE) => Probe::Workspace(UnusableWorkspace::Unwritable {
            user: config.user.clone(),
            workspace: workspace.to_path_buf(),
        }),
        _ => Probe::NotReady(text(&output.stderr)),
    }
}

/// A Docker client command: no stdin, killed when dropped, and in its own process group, so
/// that signals a terminal sends to its foreground group reach only the agent, which decides
/// what to stop. (The `docker run` client was also seen to keep running on SIGINT by itself.)
fn docker(program: &Path) -> Command {
    let mut command = Command::new(program);
    command.stdin(Stdio::null()).kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    command
}

/// Arguments for `docker run` of the container `name` that mounts `workspace`, which stays
/// attached so that its stdin ties the container's lifetime to the agent.
fn run_args(config: &ContainerConfig, workspace: &Path, name: &str) -> Vec<String> {
    let workspace = workspace.display().to_string();
    let mut args: Vec<String> = [
        "run",
        "--interactive",
        "--rm",
        "--init",
        "--sig-proxy=false",
        "--pull",
        "never",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
    ]
    .map(String::from)
    .into();
    args.extend(["--network".into(), config.network.to_string()]);
    if config.root_access == Access::ReadOnly {
        args.extend(["--read-only", "--tmpfs", "/tmp"].map(String::from));
    }
    let mount_options = match config.workspace_access {
        Access::ReadWrite => "",
        Access::ReadOnly => ",readonly",
    };
    args.extend([
        "--name".into(),
        name.into(),
        "--label".into(),
        format!("zx-agent.session={}", config.label),
        "--pids-limit".into(),
        config.pids_limit.to_string(),
        "--memory".into(),
        config.memory.clone(),
        // Only PID 1 and its `cat`; every `docker exec` names the command user.
        "--user".into(),
        KEEPER_USER.into(),
        "--mount".into(),
        format!("type=bind,source={workspace},target={workspace}{mount_options}"),
    ]);
    // No --workdir: PID 1 needs none, and an unreadable workspace would stop the container
    // from starting with Docker's own error instead of the probe's explanation.
    args.extend(["--entrypoint".into(), "cat".into(), config.image.clone()]);
    args
}

/// Check that the `bytes` read from `path` are within `max_bytes`.
fn check_read_within(path: &Path, bytes: &[u8], max_bytes: u64) -> Result<(), String> {
    let read = u64::try_from(bytes.len()).map_err(|e| format!("file length is out of range: {e}"))?;
    if read > max_bytes {
        return Err(over_limit_error(path, max_bytes));
    }
    Ok(())
}

/// Client output for an error message: lossy UTF-8, trimmed, or a note that there was none.
fn text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes).trim().to_string();
    if text.is_empty() { "(no output)".into() } else { text }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The workspace the containers of these tests mount.
    const WORK: &str = "/work";

    fn config() -> ContainerConfig {
        ContainerConfig {
            label: "s1".into(),
            ..ContainerConfig::new("debian:bookworm-slim")
        }
    }

    #[test]
    fn inside_the_walls_the_container_is_open_and_tied_to_its_client() {
        let args = run_args(&config(), Path::new(WORK), "zx-agent-x");
        let joined = args.join(" ");
        for flag in [
            "run --interactive --rm --init --sig-proxy=false --pull never",
            "--cap-drop ALL --security-opt no-new-privileges --network bridge",
            "--name zx-agent-x --label zx-agent.session=s1",
            // The keep-alive processes run as root; commands get their user on docker exec.
            "--pids-limit 256 --memory 1g --user 0:0",
            "--mount type=bind,source=/work,target=/work --entrypoint",
            "--entrypoint cat debian:bookworm-slim",
        ] {
            assert!(joined.contains(flag), "{flag:?} missing from {joined}");
        }
        // The walls: nothing shares the host's namespaces, devices or Docker daemon.
        for wall in [
            "--privileged",
            "--pid",
            "--ipc",
            "--uts",
            "--userns",
            "--device",
            "--cap-add",
        ] {
            let breached = args
                .iter()
                .any(|arg| arg == wall || arg.starts_with(&format!("{wall}=")));
            assert!(!breached, "{wall:?} in {joined}");
        }
        assert!(!joined.contains("docker.sock"), "{joined}");
        assert!(
            !joined.contains("--read-only") && !joined.contains("readonly"),
            "{joined}"
        );
        assert_eq!(args.iter().filter(|arg| *arg == "--mount").count(), 1, "{joined}");
        assert_eq!(args.last().map(String::as_str), Some("debian:bookworm-slim"));
        // No environment from the agent, and no working directory that could fail the start.
        assert!(
            !args
                .iter()
                .any(|arg| ["-e", "--env", "-w", "--workdir"].contains(&arg.as_str())),
            "{joined}"
        );
    }

    #[test]
    fn a_strict_container_has_a_read_only_workspace_and_root_and_no_network() {
        let strict = ContainerConfig {
            workspace_access: Access::ReadOnly,
            root_access: Access::ReadOnly,
            network: Network::None,
            ..config()
        };
        let joined = run_args(&strict, Path::new(WORK), "zx-agent-x").join(" ");
        for flag in [
            "--network none --read-only --tmpfs /tmp",
            "--mount type=bind,source=/work,target=/work,readonly",
        ] {
            assert!(joined.contains(flag), "{flag:?} missing from {joined}");
        }
        // Each setting on its own: a read-only workspace in a writable container.
        let joined = run_args(
            &ContainerConfig {
                workspace_access: Access::ReadOnly,
                ..config()
            },
            Path::new(WORK),
            "zx-agent-x",
        )
        .join(" ");
        assert!(joined.contains("target=/work,readonly"), "{joined}");
        assert!(!joined.contains("--read-only"), "{joined}");
    }
}
