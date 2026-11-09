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
//! concurrent call, the executor runs one command or file read at a time.
//!
//! - `config`: how the container is started and described: its settings and its network
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

use config::parse_platform;
pub use config::{Access, ContainerConfig, Network, Platform, check_network, environment_section};
use scripts::{
    EXIT_DENIED, EXIT_DIRECTORY, EXIT_MISSING, EXIT_NO_DIRECTORY, EXIT_SPECIAL, PROBE, PROBE_MISSING,
    PROBE_NO_WORKSPACE, PROBE_READ_ONLY_WORKSPACE, PROBE_ROOT, READ, RUN, SWEEP, WRITE,
};

use super::capture::{Capture, await_end, collect};
use super::{
    Captured, CommandEnd, CommandOutput, Executor, ShellCommand, check_read_within, directory_error, special_file_error,
};
use crate::agent::ExecutionMode;

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
    _client_stderr: Capture,
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
        let stderr = Capture::spawn(client.stderr.take());
        let started = Instant::now();
        let platform = loop {
            if let Some(status) = client.try_wait().context("cannot check the docker run client")? {
                bail!(
                    "docker run exited ({status}) before the container started: {}",
                    text(&stderr.finish().await.bytes)
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
            _client_stderr: stderr,
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

impl ContainerExecutor {
    /// Run the file script `command` for `path` on this executor's turn, within the time
    /// `operation` allows. `transfer` takes the pipes it needs from the started script and moves
    /// the file's bytes through them; then the script's exit status and error output are
    /// collected. Returns the status, what `transfer` returned and the error output.
    async fn file_script<T, F>(
        &self,
        mut command: Command,
        operation: &FileOperation,
        path: &Path,
        transfer: impl FnOnce(&mut Child) -> F,
    ) -> Result<(ExitStatus, T, Vec<u8>), String>
    where
        F: Future<Output = Result<T, String>>,
    {
        // A sweep for a command would also stop this script, so wait for any command to finish.
        let _turn = self.turn.lock().await;
        command.stderr(Stdio::piped());
        let script = async {
            let mut child = command.spawn().map_err(|e| format!("cannot run docker exec: {e}"))?;
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
                "{} {} in the container took longer than {}s",
                operation.doing,
                path.display(),
                operation.timeout.as_secs()
            )
        })?
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
            () = cancel.cancelled() => return Ok(not_started()),
            turn = self.turn.lock() => turn,
        };
        // An earlier run that was dropped may have left processes whose sweep is still queued.
        self.target.sweep().await;
        // Commands never inherit the agent's environment, so `hidden_env` has nothing to remove.
        // The shell is the one the start-up probe found, whatever the tool was configured with.
        let mut command = self.target.exec(
            Some(request.cwd),
            Input::Closed,
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
        let (status, bytes, stderr) = self.file_script(command, &READING, path, read).await?;
        match status.code() {
            Some(0) => {
                check_read_within(path, &bytes, max_bytes)?;
                Ok(bytes)
            }
            Some(EXIT_DIRECTORY) => Err(directory_error(path)),
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
        let (status, (), stderr) = self.file_script(command, &WRITING, path, write).await?;
        match status.code() {
            Some(0) => Ok(()),
            Some(EXIT_DIRECTORY) => Err(format!("{} is a directory", path.display())),
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
        Input::Closed,
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

/// Client output for an error message: lossy UTF-8, trimmed, or a note that there was none.
fn text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes).trim().to_string();
    if text.is_empty() { "(no output)".into() } else { text }
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
            network: Network::None,
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
            assert_eq!(user, config::NOBODY);
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
        ];
        for config in cases {
            assert!(config.validate().is_err(), "{config:?}");
        }
        assert!(config().validate().is_ok());
    }

    #[test]
    fn networks_shared_with_the_outside_are_refused() {
        for network in ["host", " HOST ", "container:other"] {
            let error = network.parse::<Network>().unwrap_err().to_string();
            assert!(error.contains("outside the sandbox"), "{network:?}: {error}");
        }
        let error = " ".parse::<Network>().unwrap_err().to_string();
        assert!(error.contains("is empty"), "{error}");
    }

    #[test]
    fn network_names_are_read_case_insensitively_and_written_back_as_docker_expects() {
        assert_eq!("bridge".parse::<Network>().unwrap(), Network::Bridge);
        assert_eq!(" None ".parse::<Network>().unwrap(), Network::None);
        assert_eq!("my-net".parse::<Network>().unwrap(), Network::Named("my-net".into()));
        assert_eq!(Network::None.to_string(), "none");
        assert_eq!(Network::Named("my-net".into()).to_string(), "my-net");
        let args = run_args(
            &ContainerConfig {
                network: Network::None,
                ..config()
            },
            "zx-agent-x",
        )
        .join(" ");
        assert!(args.contains("--network none"), "{args}");
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
            network: Network::None,
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
