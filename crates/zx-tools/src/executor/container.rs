//! Commands and files inside one Docker container, isolated from this machine.
//!
//! The container is the sandbox, and inside it the agent may do what its user may (decision D11
//! in docs/ROADMAP.md): by default the workspace and the container's own files are writable
//! and the network is Docker's default bridge. The walls stay: no privileged mode, no Docker
//! socket, no host network or PID namespace, no capabilities, none of the agent's environment.
//! A stricter sandbox, with a read-only workspace or root and no network, is a setting. Its
//! PID 1 reads the stdin of the attached `docker run` client that the
//! executor owns. Dropping the executor kills the client, and if the agent dies the pipe
//! closes; either way the container is removed (`--rm`). Docker 29.9.2 behaved this way when
//! tested on 2026-11-01; see docs/ROADMAP.md (P6).
//!
//! Commands run as a non-root user, while PID 1 and its `cat` run as root without
//! capabilities. Around every command, a sweep run as the command user kills every process
//! that user owns (`kill -1`): children that left the process group with `setsid` are
//! stopped too, nothing the command can write decides what is stopped, and the root processes
//! that keep the container alive cannot be signalled. Because a sweep would also stop a
//! concurrent call, the executor runs one command or file read at a time.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

use super::capture::{finish_reader, spawn_reader, stop_child, take_capture};
use super::{Captured, CommandEnd, CommandOutput, Executor, ShellCommand};
use crate::agent::ExecutionMode;
use crate::prompt::{pretty_name, run_started};
use crate::sync::AbortOnDrop;

/// Longest wait for a new container to accept commands.
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// Pause between readiness probes while the container starts.
const START_POLL: Duration = Duration::from_millis(100);
/// Longest wait for one readiness probe.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest wait for one file read. A FIFO swapped in after the checks would otherwise block.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest wait for one file write, for the same reason.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest wait for one sweep of the command user's processes.
const KILL_TIMEOUT: Duration = Duration::from_secs(10);
/// The user that PID 1 and its `cat` run as: root without capabilities, so that commands,
/// which must not run as root, cannot signal the processes that keep the container alive.
const KEEPER_USER: &str = "0:0";

/// Exit codes of the start-up probe for an unusable container; other failures are retried.
const PROBE_ROOT: i32 = 90;
const PROBE_MISSING: i32 = 91;
const PROBE_NO_WORKSPACE: i32 = 93;
const PROBE_READ_ONLY_WORKSPACE: i32 = 98;

/// The user commands run as when the agent itself runs as root, or off Unix: nobody.
const NOBODY: &str = "65534:65534";

/// Checks that commands run as a non-root user, that the image has the required programs
/// and that the user can read the workspace, and write it when it is mounted writable; prints
/// the platform. Arguments: the shell commands run with, the workspace, and `rw` or `ro`.
const PROBE: &str = r#"shell=$1 workspace=$2 access=$3
uid=
while read -r key value rest; do
  [ "$key" = "Uid:" ] && uid=$value
done < /proc/self/status
[ -n "$uid" ] && [ "$uid" != 0 ] || { echo "commands must not run as root (uid ${uid:-unknown})" >&2; exit 90; }
missing=
for program in "$shell" cat head uname mkdir; do
  command -v "$program" >/dev/null 2>&1 || missing="$missing $program"
done
[ -z "$missing" ] || { echo "the image lacks:$missing" >&2; exit 91; }
{ [ -r "$workspace" ] && [ -x "$workspace" ]; } || exit 93
[ "$access" = ro ] || [ -w "$workspace" ] || exit 98
echo "arch=$(uname -m)"
echo "kernel=$(uname -r)"
echo "--- os-release"
cat /etc/os-release 2>/dev/null
exit 0"#;

/// Becomes the shell. Arguments: the shell and the command.
const RUN: &str = r#"exec "$1" -c "$2""#;

/// Kills every process of the user it runs as, except itself (kill(2) with pid -1). Run as the
/// command user, this stops a command's whole tree, including children in another session.
/// `-s KILL -- -1` is the form Debian's dash accepts (tested 2026-11-01).
const SWEEP: &str = r#"kill -s KILL -- -1 2>/dev/null
exit 0"#;

/// Exit codes of the read script for each refusal.
const READ_DIRECTORY: i32 = 94;
const READ_MISSING: i32 = 95;
const READ_SPECIAL: i32 = 96;
const READ_DENIED: i32 = 97;

/// Refuses directories and special files, creates missing parent directories, then replaces
/// the file's contents with its standard input. Arguments: the absolute path.
const WRITE: &str = r#"case $1 in */?*) dir=${1%/*}; [ -z "$dir" ] || [ -d "$dir" ] || mkdir -p -- "$dir" || exit 99 ;; esac
if [ -d "$1" ]; then exit 94; fi
if [ -e "$1" ] && [ ! -f "$1" ]; then exit 96; fi
exec cat > "$1""#;

/// Exit code of the write script when it cannot create a parent directory.
const WRITE_NO_DIRECTORY: i32 = 99;

/// Refuses directories, missing, special and unreadable files; then reads a bounded prefix.
/// Arguments: the path and the byte limit.
const READ: &str = r#"if [ -d "$1" ]; then exit 94; fi
if [ ! -e "$1" ]; then exit 95; fi
if [ ! -f "$1" ]; then exit 96; fi
if [ ! -r "$1" ]; then exit 97; fi
exec head -c "$2" -- "$1""#;

/// Whether the container may change a part of its file system. In configuration files,
/// `readWrite` or `readOnly`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Access {
    #[default]
    ReadWrite,
    ReadOnly,
}

/// How to start the container. Paths are absolute host paths, mounted at the same paths.
#[derive(Clone, Debug)]
pub struct ContainerConfig {
    /// Image to run. It must already be present locally; nothing is pulled.
    pub image: String,
    /// Docker client program; looked up on `PATH` unless it is a path.
    pub docker: PathBuf,
    /// Mounted at the same path; commands start here. Everything in it is readable inside the
    /// container, and with [`Access::ReadWrite`] changeable, so keep files the container must
    /// not read or change, such as the session file, elsewhere.
    pub workspace: PathBuf,
    /// Whether commands may change the workspace, which holds the host's own files.
    pub workspace_access: Access,
    /// Whether commands may change the container's own files, such as installed packages.
    /// Changes are discarded with the container. With [`Access::ReadOnly`] only a `/tmp` in
    /// memory is writable.
    pub root_access: Access,
    /// Docker network the container joins: `bridge` (Docker's default), `none` for no
    /// network, or a user-defined network. `host` and `container:<name>` are refused: they
    /// share a network namespace outside the sandbox.
    pub network: String,
    /// Shell that commands run with; the image must provide it.
    pub shell: String,
    /// `uid:gid` that commands run as. Must not be root: the keep-alive processes run as root,
    /// and cleanup stops every process of this user.
    pub user: String,
    /// Memory limit in Docker's syntax, such as `1g`.
    pub memory: String,
    /// Maximum number of processes in the container.
    pub pids_limit: u32,
    /// Value of the `zx-agent.session` label, for example the session ID.
    pub label: String,
}

impl ContainerConfig {
    /// Settings for `image` with the defaults: a writable workspace and root, the `bridge`
    /// network, the agent's own `uid:gid` (so that files written in the workspace belong to
    /// the agent's user; nobody, `65534:65534`, when the agent runs as root or off Unix), 1 GiB
    /// of memory, 256 processes, bash, and the `docker` client found on `PATH`.
    pub fn new(image: impl Into<String>, workspace: impl Into<PathBuf>) -> Self {
        Self {
            image: image.into(),
            docker: PathBuf::from("docker"),
            workspace: workspace.into(),
            workspace_access: Access::ReadWrite,
            root_access: Access::ReadWrite,
            network: "bridge".into(),
            shell: "bash".into(),
            user: default_user(),
            memory: "1g".into(),
            pids_limit: 256,
            label: String::new(),
        }
    }

    fn validate(&self) -> Result<()> {
        ensure!(!self.image.trim().is_empty(), "container image is empty");
        ensure!(!self.shell.trim().is_empty(), "container shell is empty");
        ensure!(!self.user.trim().is_empty(), "container user is empty");
        // The start-up probe also checks the effective UID, which catches names mapped to 0.
        let uid = self.user.split(':').next().unwrap_or_default().trim();
        ensure!(
            uid != "root" && uid.parse::<u32>() != Ok(0),
            "container user {:?} is root; commands must run as an unprivileged user",
            self.user
        );
        // Profiles check it when they load; SDK callers may set the field directly.
        check_network(&self.network)?;
        ensure!(!self.memory.trim().is_empty(), "container memory limit is empty");
        ensure!(self.pids_limit > 0, "container process limit must be greater than zero");
        ensure!(
            self.workspace.is_absolute(),
            "container mount {} is not absolute",
            self.workspace.display()
        );
        // --mount separates its fields with commas.
        let text = self.workspace.to_str().context("container mount paths must be UTF-8")?;
        ensure!(
            !text.contains([',', '"', '\n']),
            "container mount {text:?} contains a comma, quote or newline"
        );
        Ok(())
    }
}

/// Refuse an empty network and the networks that would share a namespace outside the sandbox:
/// `host` and `container:<name>`. Configuration layers call this to report the mistake before
/// anything starts; [`ContainerExecutor::start`] checks it as well.
pub fn check_network(network: &str) -> Result<()> {
    let name = network.trim().to_ascii_lowercase();
    ensure!(!name.is_empty(), "container network is empty");
    ensure!(
        name != "host" && !name.starts_with("container:"),
        "container network {network:?} would share a network namespace outside the sandbox; use bridge, none or a network of your own"
    );
    Ok(())
}

/// What the container reports about itself at start.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Platform {
    /// Contents of `/etc/os-release`; empty when the image has none.
    pub os_release: String,
    /// `uname -r`: the kernel the container shares with its host.
    pub kernel: String,
    /// `uname -m`.
    pub arch: String,
}

/// Where `docker exec` runs calls: the client program, the container and the command user.
#[derive(Clone)]
struct Target {
    docker: PathBuf,
    name: String,
    user: String,
}

impl Target {
    /// `docker exec --user <user> [--workdir <dir>] <container> sh -c <script> <label> <arguments...>`.
    fn exec<I, S>(&self, workdir: Option<&Path>, script: &str, label: &str, arguments: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = docker(&self.docker);
        command.args(["exec", "--user", &self.user]);
        if let Some(workdir) = workdir {
            command.arg("--workdir").arg(workdir);
        }
        command
            .arg(&self.name)
            .args(["sh", "-c", script, label])
            .args(arguments);
        command
    }

    /// Like [`Target::exec`], with the command's standard input open for the caller to write.
    fn exec_with_input<I, S>(&self, script: &str, label: &str, arguments: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = docker(&self.docker);
        command
            .args(["exec", "--interactive", "--user", &self.user])
            .arg(&self.name)
            .args(["sh", "-c", script, label])
            .args(arguments)
            .stdin(Stdio::piped());
        command
    }

    /// Kill every process of the command user (see `SWEEP`). A failure leaves them running
    /// until the container is removed with the agent.
    async fn sweep(&self) {
        let mut command = self.exec(None, SWEEP, "zx-agent-sweep", std::iter::empty::<&str>());
        command.stdout(Stdio::null()).stderr(Stdio::null());
        // See above: there is nothing more to do when the sweep fails.
        let _ = tokio::time::timeout(KILL_TIMEOUT, command.status()).await;
    }
}

/// Runs commands and reads files in one container that lives as long as this value.
pub struct ContainerExecutor {
    target: Target,
    image: String,
    /// The shell of the configuration, checked at start; commands always run with it.
    shell: String,
    platform: Platform,
    /// Taken by one command or file read at a time, because a sweep stops every process of
    /// the command user. Shared (`Arc`) with the cleanup task of a dropped run, which outlives
    /// the run and must wait for its turn like any other call.
    turn: Arc<tokio::sync::Mutex<()>>,
    /// The attached `docker run` client: its open stdin keeps the container running, and
    /// killing it on drop removes the container.
    _client: Child,
    /// Drains the client's error output so that it never blocks on a full pipe.
    _client_stderr: Option<AbortOnDrop<()>>,
}

impl ContainerExecutor {
    /// Start the container and check that it can run commands. Fails, and removes the
    /// container, when the settings are invalid (including a root user), Docker cannot run,
    /// the image is not present locally, the container does not accept commands within 60
    /// seconds, the image lacks a required program, or commands would run as root.
    pub async fn start(config: ContainerConfig) -> Result<Self> {
        config.validate()?;
        let name = format!("zx-agent-{}", uuid::Uuid::new_v4().simple());
        let target = Target {
            docker: config.docker.clone(),
            name: name.clone(),
            user: config.user.clone(),
        };
        let mut command = docker(&config.docker);
        command
            .args(run_args(&config, &name))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut client = command
            .spawn()
            .with_context(|| format!("cannot run {}; is Docker installed?", config.docker.display()))?;
        let (stderr, mut stderr_reader) = spawn_reader(client.stderr.take());
        let started = Instant::now();
        let platform = loop {
            if let Some(status) = client.try_wait().context("cannot check the docker run client")? {
                if let Some(reader) = stderr_reader.take() {
                    finish_reader(reader).await;
                }
                bail!(
                    "docker run exited ({status}) before the container started: {}",
                    text(&take_capture(&stderr).bytes)
                );
            }
            let reason = match probe(&target, &config).await {
                Probe::Ready(platform) => break platform,
                Probe::Unusable(reason) => bail!("container image {} cannot be used: {reason}", config.image),
                Probe::NotReady(reason) => reason,
            };
            ensure!(
                started.elapsed() < START_TIMEOUT,
                "container {name} did not accept commands within {}s: {reason}",
                START_TIMEOUT.as_secs()
            );
            tokio::time::sleep(START_POLL).await;
        };
        Ok(Self {
            target,
            image: config.image,
            shell: config.shell,
            platform,
            turn: Arc::new(tokio::sync::Mutex::new(())),
            _client: client,
            _client_stderr: stderr_reader,
        })
    }

    /// What the container reported at start.
    pub fn platform(&self) -> &Platform {
        &self.platform
    }

    /// The image the container runs.
    pub fn image(&self) -> &str {
        &self.image
    }

    /// The container's name, for example to find it with `docker ps`.
    pub fn name(&self) -> &str {
        &self.target.name
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

    async fn run(&self, request: &ShellCommand<'_>, cancel: &CancellationToken) -> Result<CommandOutput, String> {
        // Declared before `cleanup`, so that a dropped run releases its turn only after the
        // cleanup task has been queued for the next one.
        let _turn = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(not_started()),
            turn = self.turn.lock() => turn,
        };
        // An earlier run that was dropped may have left processes whose sweep is still queued.
        self.target.sweep().await;
        // Commands never inherit the agent's environment, so `hidden_env` has nothing to remove.
        // The shell is the one the start-up probe found, whatever the tool was configured with.
        let mut command = self.target.exec(
            Some(request.cwd),
            RUN,
            "zx-agent-run",
            [self.shell.as_str(), request.command],
        );
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|e| format!("failed to start docker exec: {e}"))?;
        let cleanup = Cleanup {
            executor: self,
            armed: true,
        };
        let (stdout, stdout_reader) = spawn_reader(child.stdout.take());
        let (stderr, stderr_reader) = spawn_reader(child.stderr.take());

        let end = tokio::select! {
            status = child.wait() => match status {
                // docker exec reports a command ended by signal N as 128 + N.
                Ok(status) => match status.code() {
                    Some(code) => CommandEnd::Exited(code),
                    None => CommandEnd::Signaled,
                },
                Err(e) => CommandEnd::WaitFailed(e.to_string()),
            },
            _ = tokio::time::sleep(request.timeout) => CommandEnd::TimedOut,
            _ = cancel.cancelled() => CommandEnd::Cancelled,
        };
        // After a normal exit too: background children end with their command, including
        // ones that started their own session.
        cleanup.finish().await;
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
        // A sweep for a command would also stop this read, so wait for any command to finish.
        let _turn = self.turn.lock().await;
        let limit = max_bytes.saturating_add(1);
        let limit_text = limit.to_string();
        let mut command = self
            .target
            .exec(None, READ, "zx-agent-read", [path.as_os_str(), OsStr::new(&limit_text)]);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let read = async {
            let mut child = command.spawn().map_err(|e| format!("cannot run docker exec: {e}"))?;
            let (stderr, stderr_reader) = spawn_reader(child.stderr.take());
            let mut bytes = Vec::new();
            if let Some(stdout) = child.stdout.take() {
                stdout
                    .take(limit)
                    .read_to_end(&mut bytes)
                    .await
                    .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            }
            let status = child
                .wait()
                .await
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            if let Some(reader) = stderr_reader {
                finish_reader(reader).await;
            }
            Ok::<_, String>((status, bytes, take_capture(&stderr).bytes))
        };
        let (status, bytes, stderr) = tokio::time::timeout(READ_TIMEOUT, read).await.map_err(|_| {
            format!(
                "reading {} in the container took longer than {}s",
                path.display(),
                READ_TIMEOUT.as_secs()
            )
        })??;
        match status.code() {
            Some(0) => {
                let read = u64::try_from(bytes.len()).map_err(|e| format!("file length is out of range: {e}"))?;
                if read > max_bytes {
                    return Err(format!(
                        "{} is over the {max_bytes} byte limit; use bash with head, tail, or grep instead",
                        path.display()
                    ));
                }
                Ok(bytes)
            }
            Some(READ_DIRECTORY) => Err(format!("{} is a directory; use bash `ls` to list it", path.display())),
            Some(READ_MISSING) => Err(format!("cannot read {}: no such file in the container", path.display())),
            Some(READ_SPECIAL) => Err(format!(
                "{} is not a regular file; special files are not supported",
                path.display()
            )),
            Some(READ_DENIED) => Err(format!(
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
        // A sweep for a command would also stop this write, so wait for any command to finish.
        let _turn = self.turn.lock().await;
        let mut command = self.target.exec_with_input(WRITE, "zx-agent-write", [path.as_os_str()]);
        command.stdout(Stdio::null()).stderr(Stdio::piped());
        let write = async {
            let mut child = command.spawn().map_err(|e| format!("cannot run docker exec: {e}"))?;
            let (stderr, stderr_reader) = spawn_reader(child.stderr.take());
            if let Some(mut stdin) = child.stdin.take() {
                // A script that refused the path exits without reading; its status explains why.
                if let Err(error) = stdin.write_all(contents).await
                    && error.kind() != std::io::ErrorKind::BrokenPipe
                {
                    return Err(format!("cannot write {}: {error}", path.display()));
                }
                // Dropping stdin closes it, which ends `cat`.
            }
            let status = child
                .wait()
                .await
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            if let Some(reader) = stderr_reader {
                finish_reader(reader).await;
            }
            Ok::<_, String>((status, take_capture(&stderr).bytes))
        };
        let (status, stderr) = tokio::time::timeout(WRITE_TIMEOUT, write).await.map_err(|_| {
            format!(
                "writing {} in the container took longer than {}s",
                path.display(),
                WRITE_TIMEOUT.as_secs()
            )
        })??;
        match status.code() {
            Some(0) => Ok(()),
            Some(READ_DIRECTORY) => Err(format!("{} is a directory", path.display())),
            Some(READ_SPECIAL) => Err(format!(
                "{} is not a regular file; special files are not supported",
                path.display()
            )),
            Some(WRITE_NO_DIRECTORY) => Err(format!(
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
    /// The container is not accepting commands yet.
    NotReady(String),
}

async fn probe(target: &Target, config: &ContainerConfig) -> Probe {
    let mut command = target.exec(
        None,
        PROBE,
        "zx-agent-probe",
        [
            OsStr::new(&config.shell),
            config.workspace.as_os_str(),
            OsStr::new(match config.workspace_access {
                Access::ReadWrite => "rw",
                Access::ReadOnly => "ro",
            }),
        ],
    );
    let output = match tokio::time::timeout(PROBE_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => return Probe::NotReady(format!("cannot run docker exec: {e}")),
        Err(_) => {
            return Probe::NotReady(format!(
                "docker exec did not answer within {}s",
                PROBE_TIMEOUT.as_secs()
            ));
        }
    };
    match output.status.code() {
        Some(0) => Probe::Ready(parse_platform(&String::from_utf8_lossy(&output.stdout))),
        Some(PROBE_ROOT | PROBE_MISSING) => Probe::Unusable(text(&output.stderr)),
        Some(PROBE_NO_WORKSPACE) => Probe::Unusable(format!(
            "user {} cannot read the workspace {}; make it readable by others, or set container.user",
            config.user,
            config.workspace.display()
        )),
        Some(PROBE_READ_ONLY_WORKSPACE) => Probe::Unusable(format!(
            "user {} cannot write the workspace {}; set container.user to its owner, or mount it read-only (container.workspace = \"readOnly\")",
            config.user,
            config.workspace.display()
        )),
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

/// Arguments for `docker run`, which stays attached so that its stdin ties the container's
/// lifetime to the agent.
fn run_args(config: &ContainerConfig, name: &str) -> Vec<String> {
    let workspace = config.workspace.display().to_string();
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
    args.extend(["--network".into(), config.network.trim().into()]);
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

/// The agent's own `uid:gid`, so that files written in the workspace belong to its user; nobody
/// when the agent runs as root, since commands must not.
#[cfg(unix)]
fn default_user() -> String {
    // SAFETY: getuid and getgid take no arguments, always succeed and touch no memory.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    if uid == 0 {
        NOBODY.into()
    } else {
        format!("{uid}:{gid}")
    }
}

#[cfg(not(unix))]
fn default_user() -> String {
    NOBODY.into()
}

fn parse_platform(output: &str) -> Platform {
    let (head, os_release) = output.split_once("--- os-release\n").unwrap_or((output, ""));
    let field = |key: &str| {
        head.lines()
            .find_map(|line| line.strip_prefix(key))
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    Platform {
        os_release: os_release.to_string(),
        kernel: field("kernel="),
        arch: field("arch="),
    }
}

/// Client output for an error message: lossy UTF-8, trimmed, or a note that there was none.
fn text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes).trim().to_string();
    if text.is_empty() { "(no output)".into() } else { text }
}

/// The `environment` section of a prompt when commands run in the container: its system, the
/// workspace (`config.workspace`, where commands start), and what commands may change and
/// reach, as `config` sets it up.
pub fn environment_section(config: &ContainerConfig, platform: &Platform) -> String {
    let name = pretty_name(&platform.os_release).unwrap_or_else(|| "Linux".to_string());
    let workspace = match config.workspace_access {
        Access::ReadWrite => "writable; changes are made to the host's files",
        Access::ReadOnly => "read-only",
    };
    let network = match config.network.trim() {
        "none" => "without network access".to_string(),
        network => format!("with network access (Docker network {network})"),
    };
    let files = match config.root_access {
        Access::ReadWrite => "the container's own files are writable and are discarded with the container",
        Access::ReadOnly => "outside the working directory only /tmp is writable",
    };
    format!(
        "OS: {name}, Linux kernel {} ({}), in a container from image {}\nRun started: {}\n\
         Working directory: {} ({workspace})\n\
         Isolation: commands run as user {} {network}; {files}.",
        platform.kernel,
        platform.arch,
        config.image,
        run_started(),
        config.workspace.display(),
        config.user,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ContainerConfig {
        ContainerConfig {
            label: "s1".into(),
            ..ContainerConfig::new("debian:bookworm-slim", "/work")
        }
    }

    #[test]
    fn inside_the_walls_the_container_is_open_and_tied_to_its_client() {
        let args = run_args(&config(), "zx-agent-x");
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
            network: "none".into(),
            ..config()
        };
        let joined = run_args(&strict, "zx-agent-x").join(" ");
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
            "zx-agent-x",
        )
        .join(" ");
        assert!(joined.contains("target=/work,readonly"), "{joined}");
        assert!(!joined.contains("--read-only"), "{joined}");
    }

    #[cfg(unix)]
    #[test]
    fn commands_run_as_the_agents_own_user_unless_it_is_root() {
        use std::os::unix::fs::MetadataExt;
        // A new file belongs to the process's user. Its group may be the directory's, as on
        // macOS, so only the user is compared.
        let file = tempfile::NamedTempFile::new().unwrap();
        let uid = file.as_file().metadata().unwrap().uid();
        let user = ContainerConfig::new("image", "/work").user;
        if uid == 0 {
            assert_eq!(user, NOBODY);
        } else {
            assert_eq!(user.split(':').next(), Some(uid.to_string().as_str()), "{user}");
        }
    }

    #[test]
    fn invalid_settings_are_rejected_before_docker_runs() {
        let cases = [
            ContainerConfig::new(" ", "/work"),
            ContainerConfig::new("image", "work"),
            ContainerConfig::new("image", "/work,ro"),
            ContainerConfig {
                pids_limit: 0,
                ..ContainerConfig::new("image", "/work")
            },
            ContainerConfig {
                network: " ".into(),
                ..ContainerConfig::new("image", "/work")
            },
        ];
        for config in cases {
            assert!(config.validate().is_err(), "{config:?}");
        }
        assert!(config().validate().is_ok());
    }

    #[test]
    fn networks_shared_with_the_outside_are_refused() {
        for network in ["host", " HOST ", "container:other"] {
            let config = ContainerConfig {
                network: network.into(),
                ..config()
            };
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains("outside the sandbox"), "{network:?}: {error}");
        }
        for network in ["bridge", "none", "my-net"] {
            let config = ContainerConfig {
                network: network.into(),
                ..config()
            };
            assert!(config.validate().is_ok(), "{network}");
        }
    }

    #[test]
    fn commands_must_not_run_as_root() {
        for user in ["0", "0:0", "00:1000", "root", "root:root", " 0 :0"] {
            let config = ContainerConfig {
                user: user.into(),
                ..config()
            };
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains("is root"), "{user:?}: {error}");
        }
        for user in ["65534:65534", "1000", "nobody"] {
            let config = ContainerConfig {
                user: user.into(),
                ..config()
            };
            assert!(config.validate().is_ok(), "{user:?}");
        }
    }

    #[test]
    fn the_probe_output_describes_the_platform() {
        let platform = parse_platform(
            "arch=aarch64\nkernel=6.10.14-linuxkit\n--- os-release\nPRETTY_NAME=\"Debian GNU/Linux 12 (bookworm)\"\n",
        );
        assert_eq!(platform.arch, "aarch64");
        assert_eq!(platform.kernel, "6.10.14-linuxkit");
        assert_eq!(platform.os_release, "PRETTY_NAME=\"Debian GNU/Linux 12 (bookworm)\"\n");
        assert_eq!(
            parse_platform("arch=x86_64\n"),
            Platform {
                arch: "x86_64".into(),
                ..Platform::default()
            }
        );
    }

    #[test]
    fn the_environment_section_describes_the_container_and_its_isolation() {
        let platform = Platform {
            os_release: "PRETTY_NAME=\"Debian GNU/Linux 12 (bookworm)\"\n".into(),
            kernel: "6.10.14-linuxkit".into(),
            arch: "aarch64".into(),
        };
        let open = ContainerConfig {
            user: "1000:1000".into(),
            ..ContainerConfig::new("debian:bookworm-slim", "/srv/app")
        };
        let section = environment_section(&open, &platform);
        let lines: Vec<&str> = section.lines().collect();
        assert_eq!(
            lines[0],
            "OS: Debian GNU/Linux 12 (bookworm), Linux kernel 6.10.14-linuxkit (aarch64), in a container from image debian:bookworm-slim"
        );
        assert!(lines[1].starts_with("Run started: "), "{section}");
        assert_eq!(
            lines[2],
            "Working directory: /srv/app (writable; changes are made to the host's files)"
        );
        assert_eq!(
            lines[3],
            "Isolation: commands run as user 1000:1000 with network access (Docker network bridge); \
             the container's own files are writable and are discarded with the container."
        );
        let unnamed = Platform::default();
        assert!(environment_section(&open, &unnamed).starts_with("OS: Linux, Linux kernel"));
    }

    #[test]
    fn a_strict_container_is_described_as_read_only_and_offline() {
        let strict = ContainerConfig {
            user: "65534:65534".into(),
            workspace_access: Access::ReadOnly,
            root_access: Access::ReadOnly,
            network: "none".into(),
            ..ContainerConfig::new("debian:bookworm-slim", "/srv/app")
        };
        let section = environment_section(&strict, &Platform::default());
        let lines: Vec<&str> = section.lines().collect();
        assert_eq!(lines[2], "Working directory: /srv/app (read-only)");
        assert_eq!(
            lines[3],
            "Isolation: commands run as user 65534:65534 without network access; outside the working directory only /tmp is writable."
        );
    }
}
