//! Commands and files inside one Docker container, isolated from this machine.
//!
//! The container has no network, a read-only root and workspace, a writable `/tmp`, no
//! capabilities and a non-root user. Its PID 1 reads the stdin of the attached
//! `docker run` client that the executor owns. Dropping the executor kills the client, and
//! if the agent dies the pipe closes; either way the container is removed (`--rm`).
//! Docker 29.9.2 behaved this way when tested on 2026-11-01; see docs/ROADMAP.md (P6).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

use super::capture::{finish_reader, spawn_reader, stop_child, take_capture};
use super::{CommandEnd, CommandOutput, Executor, ShellCommand};
use crate::sync::AbortOnDrop;

/// Longest wait for a new container to accept commands.
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// Pause between readiness probes while the container starts.
const START_POLL: Duration = Duration::from_millis(100);
/// Longest wait for one readiness probe.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest wait for one file read. A FIFO swapped in after the checks would otherwise block.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest wait for stopping a command's process group.
const KILL_TIMEOUT: Duration = Duration::from_secs(10);
/// Where the run wrapper records process group IDs: the container's writable tmpfs.
const STATE_DIR: &str = "/tmp/.zx-agent";

/// Exit codes of the start-up probe for an unusable container; other failures are retried.
const PROBE_NO_GROUP: i32 = 90;
const PROBE_MISSING: i32 = 91;
const PROBE_NO_STATE: i32 = 92;
const PROBE_NO_WORKSPACE: i32 = 93;

/// Checks the process-group assumption, required programs and access to the workspace;
/// prints the platform. Arguments: the shell commands run with, the state directory and
/// the workspace.
const PROBE: &str = r#"shell=$1 state=$2 workspace=$3
stat=$(cat /proc/$$/stat) || { echo "cannot read /proc/$$/stat" >&2; exit 90; }
set -- $stat
[ "$1" = "$5" ] || { echo "docker exec does not start commands as process group leaders (pid $1, group $5)" >&2; exit 90; }
missing=
for program in "$shell" cat head sleep mkdir rm uname; do
  command -v "$program" >/dev/null 2>&1 || missing="$missing $program"
done
[ -z "$missing" ] || { echo "the image lacks:$missing" >&2; exit 91; }
mkdir -p "$state" || exit 92
{ [ -r "$workspace" ] && [ -x "$workspace" ]; } || exit 93
echo "arch=$(uname -m)"
echo "kernel=$(uname -r)"
echo "--- os-release"
cat /etc/os-release 2>/dev/null
exit 0"#;

/// Records the process group (this process leads it), then becomes the shell.
/// Arguments: the PID file, the shell and the command.
const RUN: &str = r#"{ echo $$ > "$1"; } 2>/dev/null || { echo "zx-agent: cannot record the command's process group" >&2; exit 125; }
exec "$2" -c "$3""#;

/// Stops a command's process group: freeze first so dying pipeline members cannot wake the
/// shell, then kill. Waits briefly for a PID file that a just-started command may still write.
/// Argument: the PID file.
const KILL: &str = r#"i=0
while [ ! -s "$1" ] && [ "$i" -lt 50 ]; do sleep 0.1; i=$((i + 1)); done
if [ -s "$1" ]; then
  group=$(cat "$1")
  kill -STOP -"$group" 2>/dev/null
  kill -KILL -"$group" 2>/dev/null
fi
rm -f "$1"
exit 0"#;

/// Exit codes of the read script for each refusal.
const READ_DIRECTORY: i32 = 94;
const READ_MISSING: i32 = 95;
const READ_SPECIAL: i32 = 96;
const READ_DENIED: i32 = 97;

/// Refuses directories, missing, special and unreadable files; then reads a bounded prefix.
/// Arguments: the path and the byte limit.
const READ: &str = r#"if [ -d "$1" ]; then exit 94; fi
if [ ! -e "$1" ]; then exit 95; fi
if [ ! -f "$1" ]; then exit 96; fi
if [ ! -r "$1" ]; then exit 97; fi
exec head -c "$2" -- "$1""#;

/// How to start the container. Paths are absolute host paths, mounted at the same paths.
#[derive(Clone, Debug)]
pub struct ContainerConfig {
    /// Image to run. It must already be present locally; nothing is pulled.
    pub image: String,
    /// Docker client program; looked up on `PATH` unless it is a path.
    pub docker: PathBuf,
    /// Mounted read-only at the same path; commands start here.
    pub workspace: PathBuf,
    /// Existing files inside the workspace that the container must not read, such as the
    /// session file. Each one appears empty inside the container.
    pub hidden_files: Vec<PathBuf>,
    /// Shell that commands run with; the image must provide it.
    pub shell: String,
    /// `uid:gid` that commands run as.
    pub user: String,
    /// Memory limit in Docker's syntax, such as `1g`.
    pub memory: String,
    /// Maximum number of processes in the container.
    pub pids_limit: u32,
    /// Value of the `zx-agent.session` label, for example the session ID.
    pub label: String,
}

impl ContainerConfig {
    /// Settings for `image` with the defaults: user nobody (`65534:65534`), 1 GiB of memory,
    /// 256 processes, bash, and the `docker` client found on `PATH`.
    pub fn new(image: impl Into<String>, workspace: impl Into<PathBuf>) -> Self {
        Self {
            image: image.into(),
            docker: PathBuf::from("docker"),
            workspace: workspace.into(),
            hidden_files: Vec::new(),
            shell: "bash".into(),
            user: "65534:65534".into(),
            memory: "1g".into(),
            pids_limit: 256,
            label: String::new(),
        }
    }

    fn validate(&self) -> Result<()> {
        ensure!(!self.image.trim().is_empty(), "container image is empty");
        ensure!(!self.shell.trim().is_empty(), "container shell is empty");
        ensure!(!self.user.trim().is_empty(), "container user is empty");
        ensure!(!self.memory.trim().is_empty(), "container memory limit is empty");
        ensure!(self.pids_limit > 0, "container process limit must be greater than zero");
        for path in std::iter::once(&self.workspace).chain(&self.hidden_files) {
            ensure!(path.is_absolute(), "container mount {} is not absolute", path.display());
            // --mount separates its fields with commas.
            let text = path.to_str().context("container mount paths must be UTF-8")?;
            ensure!(
                !text.contains([',', '"', '\n']),
                "container mount {text:?} contains a comma, quote or newline"
            );
        }
        Ok(())
    }
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

/// Runs commands and reads files in one container that lives as long as this value.
pub struct ContainerExecutor {
    docker: PathBuf,
    name: String,
    image: String,
    platform: Platform,
    /// The attached `docker run` client: its open stdin keeps the container running, and
    /// killing it on drop removes the container.
    _client: Child,
    /// Drains the client's error output so that it never blocks on a full pipe.
    _client_stderr: Option<AbortOnDrop<()>>,
}

impl ContainerExecutor {
    /// Start the container and check that it can run commands. Fails, and removes the
    /// container, when the settings are invalid, Docker cannot run, the image is not present
    /// locally, the container does not accept commands within 60 seconds, or the image lacks
    /// a required program.
    pub async fn start(config: ContainerConfig) -> Result<Self> {
        config.validate()?;
        let name = format!("zx-agent-{}", uuid::Uuid::new_v4().simple());
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
            let reason = match probe(&config, &name).await {
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
            docker: config.docker,
            name,
            image: config.image,
            platform,
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
        &self.name
    }

    /// `docker exec <container> sh -c <script> <label> <arguments...>`.
    fn exec<I, S>(&self, workdir: Option<&Path>, script: &str, label: &str, arguments: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = docker(&self.docker);
        command.arg("exec");
        if let Some(workdir) = workdir {
            command.arg("--workdir").arg(workdir);
        }
        command
            .arg(&self.name)
            .args(["sh", "-c", script, label])
            .args(arguments);
        command
    }
}

#[async_trait]
impl Executor for ContainerExecutor {
    async fn run(&self, request: &ShellCommand<'_>, cancel: &CancellationToken) -> Result<CommandOutput, String> {
        // Commands never inherit the agent's environment, so `hidden_env` has nothing to remove.
        let pid_file = format!("{STATE_DIR}/{}", uuid::Uuid::new_v4().simple());
        let mut command = self.exec(
            Some(request.cwd),
            RUN,
            "zx-agent-run",
            [pid_file.as_str(), request.shell, request.command],
        );
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|e| format!("failed to start docker exec: {e}"))?;
        let group = GroupGuard {
            executor: self,
            pid_file,
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
        // Stop the group after a normal exit too, ending background children.
        group.stop().await;
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
        let limit = max_bytes.saturating_add(1);
        let limit_text = limit.to_string();
        let mut command = self.exec(None, READ, "zx-agent-read", [path.as_os_str(), OsStr::new(&limit_text)]);
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
}

/// Stops a command's process group in the container, including when the run future is
/// dropped before the command ends.
struct GroupGuard<'a> {
    executor: &'a ContainerExecutor,
    pid_file: String,
    armed: bool,
}

impl GroupGuard<'_> {
    fn command(&self) -> Command {
        let mut command = self
            .executor
            .exec(None, KILL, "zx-agent-kill", [self.pid_file.as_str()]);
        command.stdout(Stdio::null()).stderr(Stdio::null());
        command
    }

    async fn stop(mut self) {
        self.armed = false;
        let mut command = self.command();
        // Failure leaves the command running until the container is removed with the agent.
        let _ = tokio::time::timeout(KILL_TIMEOUT, command.status()).await;
    }
}

impl Drop for GroupGuard<'_> {
    fn drop(&mut self) {
        // Spawning a Tokio process needs a runtime; without one the container's removal
        // with the agent is the only cleanup.
        if !self.armed || tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let mut command = self.command();
        // The cleanup must outlive the dropped run; Tokio reaps the client in the background
        // (tokio 1.54 src/process/mod.rs:218). Like a failed stop, a failed spawn leaves the
        // command running until the container is removed.
        command.kill_on_drop(false);
        let _ = command.spawn();
    }
}

enum Probe {
    Ready(Platform),
    /// The image cannot be used; retrying will not help.
    Unusable(String),
    /// The container is not accepting commands yet.
    NotReady(String),
}

async fn probe(config: &ContainerConfig, name: &str) -> Probe {
    let mut command = docker(&config.docker);
    command
        .args(["exec", name, "sh", "-c", PROBE, "zx-agent-probe"])
        .arg(&config.shell)
        .arg(STATE_DIR)
        .arg(&config.workspace);
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
        Some(PROBE_NO_GROUP | PROBE_MISSING | PROBE_NO_STATE) => Probe::Unusable(text(&output.stderr)),
        Some(PROBE_NO_WORKSPACE) => Probe::Unusable(format!(
            "user {} cannot read the workspace {}; make it readable by others, or set container.user",
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
        "--network",
        "none",
        "--read-only",
        "--tmpfs",
        "/tmp",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
    ]
    .map(String::from)
    .into();
    args.extend([
        "--name".into(),
        name.into(),
        "--label".into(),
        format!("zx-agent.session={}", config.label),
        "--pids-limit".into(),
        config.pids_limit.to_string(),
        "--memory".into(),
        config.memory.clone(),
        "--user".into(),
        config.user.clone(),
        "--mount".into(),
        format!("type=bind,source={workspace},target={workspace},readonly"),
    ]);
    for hidden in &config.hidden_files {
        args.push("--mount".into());
        args.push(format!(
            "type=bind,source=/dev/null,target={},readonly",
            hidden.display()
        ));
    }
    // No --workdir: PID 1 needs none, and an unreadable workspace would stop the container
    // from starting with Docker's own error instead of the probe's explanation.
    args.extend(["--entrypoint".into(), "cat".into(), config.image.clone()]);
    args
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ContainerConfig {
        ContainerConfig {
            hidden_files: vec![PathBuf::from("/work/session.jsonl")],
            label: "s1".into(),
            ..ContainerConfig::new("debian:bookworm-slim", "/work")
        }
    }

    #[test]
    fn the_container_is_hardened_and_tied_to_its_client() {
        let args = run_args(&config(), "zx-agent-x");
        let joined = args.join(" ");
        for flag in [
            "run --interactive --rm --init --sig-proxy=false --pull never",
            "--network none --read-only --tmpfs /tmp --cap-drop ALL --security-opt no-new-privileges",
            "--name zx-agent-x --label zx-agent.session=s1",
            "--pids-limit 256 --memory 1g --user 65534:65534",
            "--mount type=bind,source=/work,target=/work,readonly",
            "--mount type=bind,source=/dev/null,target=/work/session.jsonl,readonly",
            "--entrypoint cat debian:bookworm-slim",
        ] {
            assert!(joined.contains(flag), "{flag:?} missing from {joined}");
        }
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
                hidden_files: vec![PathBuf::from("relative.jsonl")],
                ..ContainerConfig::new("image", "/work")
            },
        ];
        for config in cases {
            assert!(config.validate().is_err(), "{config:?}");
        }
        assert!(config().validate().is_ok());
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
}
