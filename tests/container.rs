//! The container executor. Tests with a fake `docker` program always run and need no Docker.
//! Tests against real Docker run only when `ZX_DOCKER_TEST_IMAGE` names a local image that
//! has bash; CI pulls one on Linux. Without it they print a note and return.

#![cfg(unix)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use zx_agent::executor::{CommandEnd, CommandOutput, ContainerConfig, ContainerExecutor, Executor, ShellCommand};

use common::mock_server::{MockServer, Reply};

// ---------------------------------------------------------------------------
// Fake docker
// ---------------------------------------------------------------------------

/// Logs every invocation's arguments to `calls/` and answers from files next to it:
/// `run.fail` (docker run fails with this message), `probe.code` and `probe.err` (the start-up
/// probe fails), `cmd.out`, `cmd.err`, `cmd.code` and `cmd.hang` (a command's result, or a
/// command that never ends), `read.out` and `read.code` (a file read).
const FAKE_DOCKER: &str = r#"#!/bin/sh
dir=$(cd "$(dirname "$0")" && pwd)
call=$(mktemp "$dir/calls/call.XXXXXX")
for arg in "$@"; do printf '%s\0' "$arg" >> "$call"; done
case "$1" in
  run)
    if [ -f "$dir/run.fail" ]; then cat "$dir/run.fail" >&2; exit 125; fi
    echo $$ > "$dir/run.pid"
    exec cat
    ;;
  exec)
    shift
    # Skip the options before the container name: --user, and --workdir for commands.
    while [ "${1#--}" != "$1" ]; do shift 2; done
    shift 4
    label=$1
    case "$label" in
      zx-agent-probe)
        # Like Docker, exec fails until the container runs.
        if [ ! -f "$dir/run.pid" ]; then echo "container is not running" >&2; exit 1; fi
        if [ -f "$dir/probe.code" ]; then cat "$dir/probe.err" >&2; exit "$(cat "$dir/probe.code")"; fi
        printf 'arch=x86_64\nkernel=6.0.0-fake\n--- os-release\nPRETTY_NAME="Fake Linux 1"\n'
        exit 0 ;;
      zx-agent-run)
        if [ -f "$dir/cmd.hang" ]; then exec sleep 30; fi
        if [ -f "$dir/cmd.out" ]; then cat "$dir/cmd.out"; fi
        if [ -f "$dir/cmd.err" ]; then cat "$dir/cmd.err" >&2; fi
        exit "$(cat "$dir/cmd.code" 2>/dev/null || echo 0)" ;;
      zx-agent-read)
        if [ -f "$dir/read.out" ]; then cat "$dir/read.out"; fi
        exit "$(cat "$dir/read.code" 2>/dev/null || echo 0)" ;;
      zx-agent-sweep)
        exit 0 ;;
    esac
    echo "fake docker: unexpected exec $label" >&2
    exit 2 ;;
esac
echo "fake docker: unexpected $1" >&2
exit 2
"#;

struct FakeDocker {
    dir: tempfile::TempDir,
    workspace: tempfile::TempDir,
}

impl FakeDocker {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("calls")).unwrap();
        let program = dir.path().join("docker");
        std::fs::write(&program, FAKE_DOCKER).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            dir,
            workspace: tempfile::tempdir().unwrap(),
        }
    }

    fn workspace(&self) -> PathBuf {
        self.workspace.path().canonicalize().unwrap()
    }

    fn config(&self) -> ContainerConfig {
        ContainerConfig {
            docker: self.dir.path().join("docker"),
            ..ContainerConfig::new("fake:1", self.workspace())
        }
    }

    fn answer(&self, file: &str, text: &str) {
        std::fs::write(self.dir.path().join(file), text).unwrap();
    }

    /// Every invocation so far, as its arguments.
    fn calls(&self) -> Vec<Vec<String>> {
        std::fs::read_dir(self.dir.path().join("calls"))
            .unwrap()
            .map(|entry| {
                let bytes = std::fs::read(entry.unwrap().path()).unwrap();
                String::from_utf8(bytes)
                    .unwrap()
                    .split_terminator('\0')
                    .map(String::from)
                    .collect()
            })
            .collect()
    }

    /// Invocations of `docker exec` for the script labelled `label`.
    fn execs(&self, label: &str) -> Vec<Vec<String>> {
        self.calls()
            .into_iter()
            .filter(|call| call.first().map(String::as_str) == Some("exec") && call.iter().any(|arg| arg == label))
            .collect()
    }
}

/// The arguments after `label` in an exec call.
fn after<'a>(call: &'a [String], label: &str) -> &'a [String] {
    let position = call.iter().position(|arg| arg == label).unwrap();
    &call[position + 1..]
}

async fn run(executor: &dyn Executor, workspace: &Path, command: &str, timeout: Duration) -> CommandOutput {
    let request = ShellCommand {
        shell: "bash",
        command,
        cwd: workspace,
        timeout,
        hidden_env: &[],
    };
    executor.run(&request, &CancellationToken::new()).await.unwrap()
}

fn stdout(output: &CommandOutput) -> String {
    String::from_utf8_lossy(&output.stdout.bytes).into_owned()
}

/// Wait up to `limit` for `ready`, checking without sleeping between attempts.
async fn eventually(limit: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if ready() {
            return true;
        }
        tokio::task::yield_now().await;
    }
    ready()
}

#[tokio::test]
async fn start_runs_a_hardened_container_and_reports_its_platform() {
    let fake = FakeDocker::new();

    let executor = ContainerExecutor::start(fake.config()).await.unwrap();

    assert!(executor.platform().os_release.contains("Fake Linux 1"));
    assert_eq!(executor.platform().arch, "x86_64");
    let calls = fake.calls();
    let run = calls.iter().find(|call| call[0] == "run").unwrap().join(" ");
    // The keep-alive processes run as root; the probe, like every call, as the command user.
    for flag in ["--network none", "--read-only", "--user 0:0", "--pull never"] {
        assert!(run.contains(flag), "{flag:?} missing from {run}");
    }
    let probe = &fake.execs("zx-agent-probe")[0];
    assert_eq!(probe[1..4], ["--user", "65534:65534", executor.name()]);
    assert_eq!(
        after(probe, "zx-agent-probe"),
        ["bash".to_string(), fake.workspace().display().to_string()]
    );
}

#[tokio::test]
async fn start_refuses_a_root_user_before_docker_runs() {
    let fake = FakeDocker::new();
    let config = ContainerConfig {
        user: "0:0".into(),
        ..fake.config()
    };

    let error = ContainerExecutor::start(config).await.err().unwrap().to_string();

    assert!(error.contains("is root"), "{error}");
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn start_refuses_an_image_whose_commands_would_run_as_root() {
    let fake = FakeDocker::new();
    fake.answer("probe.code", "90");
    fake.answer("probe.err", "commands must not run as root (uid 0)");

    let error = ContainerExecutor::start(fake.config()).await.err().unwrap().to_string();

    assert_eq!(
        error,
        "container image fake:1 cannot be used: commands must not run as root (uid 0)"
    );
}

#[tokio::test]
async fn start_reports_why_docker_run_failed() {
    let fake = FakeDocker::new();
    fake.answer("run.fail", "Unable to find image 'fake:1' locally");

    let error = ContainerExecutor::start(fake.config()).await.err().unwrap().to_string();

    assert!(error.contains("docker run exited"), "{error}");
    assert!(error.contains("Unable to find image 'fake:1' locally"), "{error}");
}

#[tokio::test]
async fn start_rejects_an_image_that_lacks_a_required_program() {
    let fake = FakeDocker::new();
    fake.answer("probe.code", "91");
    fake.answer("probe.err", "the image lacks: bash");

    let error = ContainerExecutor::start(fake.config()).await.err().unwrap().to_string();

    assert_eq!(error, "container image fake:1 cannot be used: the image lacks: bash");
}

#[tokio::test]
async fn start_explains_a_workspace_the_container_user_cannot_read() {
    let fake = FakeDocker::new();
    fake.answer("probe.code", "93");
    fake.answer("probe.err", "");

    let error = ContainerExecutor::start(fake.config()).await.err().unwrap().to_string();

    assert_eq!(
        error,
        format!(
            "container image fake:1 cannot be used: user 65534:65534 cannot read the workspace {}; make it readable by others, or set container.user",
            fake.workspace().display()
        )
    );
}

#[tokio::test]
async fn commands_run_as_their_user_in_the_workspace_between_two_sweeps() {
    let fake = FakeDocker::new();
    fake.answer("cmd.out", "hi\n");
    fake.answer("cmd.err", "warn\n");
    fake.answer("cmd.code", "3");
    let executor = ContainerExecutor::start(fake.config()).await.unwrap();

    let output = run(&executor, &fake.workspace(), "echo hi", Duration::from_secs(10)).await;

    assert_eq!(stdout(&output), "hi\n");
    assert_eq!(output.stderr.bytes, b"warn\n");
    assert!(matches!(output.end, CommandEnd::Exited(3)), "{:?}", output.end);
    let call = &fake.execs("zx-agent-run")[0];
    assert_eq!(
        call[1..5],
        [
            "--user".to_string(),
            "65534:65534".to_string(),
            "--workdir".to_string(),
            fake.workspace().display().to_string()
        ]
    );
    assert_eq!(after(call, "zx-agent-run"), ["bash".to_string(), "echo hi".to_string()]);
    assert!(!call.iter().any(|arg| arg == "-e" || arg == "--env"), "{call:?}");
    // One sweep clears what an earlier call may have left, one stops what this one left.
    let sweeps = fake.execs("zx-agent-sweep");
    assert_eq!(sweeps.len(), 2, "{sweeps:?}");
    assert!(sweeps.iter().all(|sweep| sweep[1..3] == ["--user", "65534:65534"]));
}

#[tokio::test]
async fn a_timed_out_command_is_stopped_inside_the_container() {
    let fake = FakeDocker::new();
    fake.answer("cmd.hang", "");
    let executor = ContainerExecutor::start(fake.config()).await.unwrap();

    let output = run(&executor, &fake.workspace(), "sleep 30", Duration::from_millis(100)).await;

    assert!(matches!(output.end, CommandEnd::TimedOut), "{:?}", output.end);
    assert_eq!(fake.execs("zx-agent-sweep").len(), 2);
}

#[tokio::test]
async fn a_cancelled_command_is_stopped_inside_the_container() {
    let fake = FakeDocker::new();
    fake.answer("cmd.hang", "");
    let executor = ContainerExecutor::start(fake.config()).await.unwrap();
    let cancel = CancellationToken::new();
    let workspace = fake.workspace();
    let request = ShellCommand {
        shell: "bash",
        command: "sleep 30",
        cwd: &workspace,
        timeout: Duration::from_secs(60),
        hidden_env: &[],
    };

    let running = executor.run(&request, &cancel);
    let cancelling = async {
        assert!(eventually(Duration::from_secs(5), || fake.execs("zx-agent-run").len() == 1).await);
        cancel.cancel();
    };
    let (output, ()) = tokio::join!(running, cancelling);

    assert!(matches!(output.unwrap().end, CommandEnd::Cancelled));
    assert_eq!(fake.execs("zx-agent-sweep").len(), 2);
}

#[tokio::test]
async fn a_run_cancelled_before_it_starts_runs_nothing() {
    let fake = FakeDocker::new();
    let executor = ContainerExecutor::start(fake.config()).await.unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let workspace = fake.workspace();
    let request = ShellCommand {
        shell: "bash",
        command: "echo never",
        cwd: &workspace,
        timeout: Duration::from_secs(60),
        hidden_env: &[],
    };

    let output = executor.run(&request, &cancel).await.unwrap();

    assert!(matches!(output.end, CommandEnd::Cancelled), "{:?}", output.end);
    assert!(fake.execs("zx-agent-run").is_empty());
}

#[tokio::test]
async fn a_dropped_run_still_stops_its_command() {
    let fake = FakeDocker::new();
    fake.answer("cmd.hang", "");
    let executor = ContainerExecutor::start(fake.config()).await.unwrap();

    let dropped = tokio::time::timeout(
        Duration::from_millis(200),
        run(&executor, &fake.workspace(), "sleep 30", Duration::from_secs(60)),
    )
    .await;

    assert!(dropped.is_err(), "the command should still have been running");
    // The sweep before the command, and the one queued when its run was dropped.
    assert!(
        eventually(Duration::from_secs(5), || fake.execs("zx-agent-sweep").len() == 2).await,
        "no cleanup after the run future was dropped"
    );
}

#[tokio::test]
async fn file_reads_map_each_refusal_to_an_actionable_error() {
    let fake = FakeDocker::new();
    let executor = ContainerExecutor::start(fake.config()).await.unwrap();
    let path = Path::new("/srv/app.log");
    for (code, message) in [
        ("94", "/srv/app.log is a directory; use bash `ls` to list it"),
        ("95", "cannot read /srv/app.log: no such file in the container"),
        (
            "96",
            "/srv/app.log is not a regular file; special files are not supported",
        ),
        ("97", "cannot read /srv/app.log: permission denied in the container"),
        ("1", "cannot read /srv/app.log in the container: (no output)"),
    ] {
        fake.answer("read.code", code);
        assert_eq!(executor.read_file(path, 10).await.unwrap_err(), message);
    }
    fake.answer("read.code", "0");
    fake.answer("read.out", "0123456789");
    assert_eq!(executor.read_file(path, 10).await.unwrap(), b"0123456789");
    assert_eq!(
        executor.read_file(path, 9).await.unwrap_err(),
        "/srv/app.log is over the 9 byte limit; use bash with head, tail, or grep instead"
    );
    // Each read asks for one byte more than its limit. Calls are listed in no particular order.
    let mut limits: Vec<String> = fake
        .execs("zx-agent-read")
        .iter()
        .map(|call| {
            let arguments = after(call, "zx-agent-read");
            assert_eq!(arguments[0], "/srv/app.log");
            arguments[1].clone()
        })
        .collect();
    limits.sort();
    assert_eq!(limits, ["10", "11", "11", "11", "11", "11", "11"]);
}

#[tokio::test]
async fn dropping_the_executor_ends_the_docker_run_client() {
    let fake = FakeDocker::new();
    let executor = ContainerExecutor::start(fake.config()).await.unwrap();
    let pid = std::fs::read_to_string(fake.dir.path().join("run.pid")).unwrap();
    let alive = || {
        std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    };
    assert!(alive());

    drop(executor);

    assert!(
        eventually(Duration::from_secs(5), || !alive()).await,
        "the docker run client outlived the executor"
    );
}

// ---------------------------------------------------------------------------
// Real Docker (ZX_DOCKER_TEST_IMAGE)
// ---------------------------------------------------------------------------

/// The image for real Docker tests, or None to skip them.
fn docker_image() -> Option<String> {
    let image = std::env::var("ZX_DOCKER_TEST_IMAGE")
        .ok()
        .filter(|image| !image.is_empty());
    if image.is_none() {
        eprintln!("skipped: set ZX_DOCKER_TEST_IMAGE to a local image with bash to run this test against Docker");
    }
    image
}

/// A temporary workspace that others can read, like a typical checkout. On native Linux the
/// container's default user does not own it, and tempfile creates directories with mode 0700.
fn real_workspace() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = dir.path().canonicalize().unwrap();
    (dir, path)
}

async fn start_real(image: &str, workspace: &Path) -> ContainerExecutor {
    ContainerExecutor::start(ContainerConfig {
        label: "zx-agent-test".into(),
        ..ContainerConfig::new(image, workspace)
    })
    .await
    .unwrap()
}

/// How many processes in the container run `sleep 30`.
const COUNT_SLEEPERS: &str =
    r#"for f in /proc/[0-9]*/cmdline; do tr '\0' ' ' < "$f" 2>/dev/null; echo; done | grep -c '^sleep 30'"#;

#[tokio::test]
async fn real_commands_run_as_nobody_in_a_read_only_workspace_without_network() {
    let Some(image) = docker_image() else { return };
    let (_dir, workspace) = real_workspace();
    let executor = start_real(&image, &workspace).await;

    let output = run(
        &executor,
        &workspace,
        "id -u; touch new 2>&1; touch /tmp/ok && echo tmp-writable; ls /sys/class/net",
        Duration::from_secs(30),
    )
    .await;

    let text = stdout(&output);
    assert!(text.starts_with("65534\n"), "{text}");
    assert!(text.contains("Read-only file system"), "{text}");
    assert!(text.contains("tmp-writable"), "{text}");
    assert!(text.contains("lo"), "{text}");
    assert!(!text.contains("eth0"), "{text}");
    assert!(!executor.platform().arch.is_empty());
}

#[tokio::test]
async fn real_a_timeout_stops_the_whole_command_tree() {
    let Some(image) = docker_image() else { return };
    let (_dir, workspace) = real_workspace();
    let executor = start_real(&image, &workspace).await;

    let output = run(&executor, &workspace, "sleep 30 | cat", Duration::from_secs(1)).await;

    assert!(matches!(output.end, CommandEnd::TimedOut), "{:?}", output.end);
    let left = run(&executor, &workspace, COUNT_SLEEPERS, Duration::from_secs(30)).await;
    assert_eq!(stdout(&left).trim(), "0");
}

#[tokio::test]
async fn real_background_children_end_with_their_command() {
    let Some(image) = docker_image() else { return };
    let (_dir, workspace) = real_workspace();
    let executor = start_real(&image, &workspace).await;

    let output = run(
        &executor,
        &workspace,
        "sleep 30 & echo started",
        Duration::from_secs(30),
    )
    .await;

    assert!(matches!(output.end, CommandEnd::Exited(0)), "{:?}", output.end);
    assert_eq!(stdout(&output), "started\n");
    let left = run(&executor, &workspace, COUNT_SLEEPERS, Duration::from_secs(30)).await;
    assert_eq!(stdout(&left).trim(), "0");
}

#[tokio::test]
async fn real_a_child_that_left_the_process_group_ends_with_its_command() {
    let Some(image) = docker_image() else { return };
    let (_dir, workspace) = real_workspace();
    let executor = start_real(&image, &workspace).await;

    // setsid gives the child its own session and process group.
    let output = run(
        &executor,
        &workspace,
        "setsid sleep 30 & echo started",
        Duration::from_secs(30),
    )
    .await;

    assert!(matches!(output.end, CommandEnd::Exited(0)), "{:?}", output.end);
    let left = run(&executor, &workspace, COUNT_SLEEPERS, Duration::from_secs(30)).await;
    assert_eq!(stdout(&left).trim(), "0");
}

#[tokio::test]
async fn real_a_command_cannot_disable_its_own_cleanup() {
    let Some(image) = docker_image() else { return };
    let (_dir, workspace) = real_workspace();
    let executor = start_real(&image, &workspace).await;

    // Everything a command can write, such as /tmp, must be irrelevant to stopping it.
    let output = run(
        &executor,
        &workspace,
        "sleep 30 & rm -rf /tmp/.zx-agent /tmp/* 2>/dev/null; echo started",
        Duration::from_secs(30),
    )
    .await;

    assert!(matches!(output.end, CommandEnd::Exited(0)), "{:?}", output.end);
    let left = run(&executor, &workspace, COUNT_SLEEPERS, Duration::from_secs(30)).await;
    assert_eq!(stdout(&left).trim(), "0");
}

#[tokio::test]
async fn real_commands_in_one_container_run_one_at_a_time() {
    let Some(image) = docker_image() else { return };
    let (_dir, workspace) = real_workspace();
    let executor = start_real(&image, &workspace).await;

    // Each command holds a lock directory for a second; an overlap makes one of them fail.
    let command = "mkdir /tmp/held || exit 9; sleep 1; rmdir /tmp/held";
    let (first, second) = tokio::join!(
        run(&executor, &workspace, command, Duration::from_secs(30)),
        run(&executor, &workspace, command, Duration::from_secs(30)),
    );

    assert!(matches!(first.end, CommandEnd::Exited(0)), "{:?}", first.end);
    assert!(matches!(second.end, CommandEnd::Exited(0)), "{:?}", second.end);
}

#[tokio::test]
async fn real_a_dropped_run_stops_its_command() {
    let Some(image) = docker_image() else { return };
    let (_dir, workspace) = real_workspace();
    let executor = start_real(&image, &workspace).await;

    let dropped = tokio::time::timeout(
        Duration::from_secs(2),
        run(&executor, &workspace, "sleep 30", Duration::from_secs(60)),
    )
    .await;

    assert!(dropped.is_err());
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let left = run(&executor, &workspace, COUNT_SLEEPERS, Duration::from_secs(30)).await;
        if stdout(&left).trim() == "0" {
            break;
        }
        assert!(Instant::now() < deadline, "the dropped command kept running");
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn real_an_unreadable_workspace_is_refused_at_start() {
    let Some(image) = docker_image() else { return };
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let workspace = dir.path().canonicalize().unwrap();

    let error = ContainerExecutor::start(ContainerConfig::new(&image, &workspace))
        .await
        .err()
        .unwrap()
        .to_string();

    assert!(error.contains("cannot read the workspace"), "{error}");
}

#[tokio::test]
async fn real_files_are_read_inside_the_container() {
    let Some(image) = docker_image() else { return };
    let (_dir, workspace) = real_workspace();
    let executor = start_real(&image, &workspace).await;

    let release = executor.read_file(Path::new("/etc/os-release"), 1 << 20).await.unwrap();
    assert!(String::from_utf8_lossy(&release).contains("NAME="));
    for (path, message) in [
        ("/tmp", "is a directory"),
        ("/no/such/file", "no such file in the container"),
        ("/dev/null", "is not a regular file"),
    ] {
        let error = executor.read_file(Path::new(path), 1 << 20).await.unwrap_err();
        assert!(error.contains(message), "{path}: {error}");
    }
    let error = executor.read_file(Path::new("/etc/os-release"), 4).await.unwrap_err();
    assert!(error.contains("over the 4 byte limit"), "{error}");
}

#[tokio::test]
async fn real_dropping_the_executor_removes_the_container() {
    let Some(image) = docker_image() else { return };
    let (_dir, workspace) = real_workspace();
    let executor = start_real(&image, &workspace).await;
    let name = executor.name().to_string();
    let exists = || {
        let output = std::process::Command::new("docker")
            .args(["ps", "--all", "--quiet", "--filter", &format!("name=^{name}$")])
            .output()
            .unwrap();
        !output.stdout.is_empty()
    };
    assert!(exists());

    drop(executor);

    let deadline = Instant::now() + Duration::from_secs(15);
    while exists() {
        assert!(Instant::now() < deadline, "container {name} outlived its executor");
        tokio::task::yield_now().await;
    }
}

// ---------------------------------------------------------------------------
// The CLI in container mode
// ---------------------------------------------------------------------------

/// `zx-agent run` in `dir` against the mock server. `docker_dir` goes first on PATH, so that
/// its `docker` replaces the real one.
async fn zx_run(server: &MockServer, dir: &Path, docker_dir: Option<&Path>, args: &[&str]) -> Output {
    const KEY_VAR: &str = "ZX_P6_CONTAINER_TEST_KEY";
    let mut command = Command::new(env!("CARGO_BIN_EXE_zx-agent"));
    command
        .args(["run", "--model", "mock", "--base-url", &server.base_url])
        .args(["--api-key-env", KEY_VAR, "--max-retries", "0"])
        .args(args)
        .current_dir(dir)
        .env(KEY_VAR, "sk-p6-container-test")
        .env_remove("ZX_MODEL")
        .env_remove("ZX_BASE_URL")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    if let Some(docker_dir) = docker_dir {
        let path = std::env::var("PATH").unwrap_or_default();
        command.env("PATH", format!("{}:{path}", docker_dir.display()));
    }
    tokio::time::timeout(Duration::from_secs(120), command.output())
        .await
        .unwrap()
        .unwrap()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The leading system message of the first model request.
fn system_prompt(server: &MockServer) -> String {
    server.requests()[0].body["messages"][0]["content"]
        .as_str()
        .unwrap()
        .to_string()
}

/// The first tool result in model request `index`.
fn tool_result(server: &MockServer, index: usize) -> String {
    server.requests()[index].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn run_in_container_mode_describes_the_container_and_runs_bash_there() {
    let fake = FakeDocker::new();
    fake.answer("cmd.out", "from-container\n");
    let workspace = fake.workspace();
    let sessions = tempfile::tempdir().unwrap();
    let session = sessions.path().join("session.jsonl");
    let server = MockServer::start(vec![Reply::bash("c1", "echo hi"), Reply::text("done")]).await;

    let output = zx_run(
        &server,
        &workspace,
        Some(fake.dir.path()),
        &[
            "--execution",
            "container",
            "--container-image",
            "fake:1",
            "--tools",
            "bash",
            "--session",
            session.to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    let prompt = system_prompt(&server);
    assert!(
        prompt.contains("OS: Fake Linux 1, Linux kernel 6.0.0-fake (x86_64), in a container from image fake:1"),
        "{prompt}"
    );
    assert!(
        prompt.contains(&format!("Working directory: {} (read-only)", workspace.display())),
        "{prompt}"
    );
    assert!(tool_result(&server, 1).starts_with("from-container\n"));
    let exec = &fake.execs("zx-agent-run")[0];
    assert_eq!(exec[3..5], ["--workdir".to_string(), workspace.display().to_string()]);
    assert_eq!(after(exec, "zx-agent-run")[1], "echo hi");
    assert!(session.exists());
}

#[tokio::test]
async fn container_execution_refuses_a_session_file_the_container_could_read() {
    let fake = FakeDocker::new();
    let workspace = fake.workspace();
    let server = MockServer::start(vec![]).await;

    // Relative to the working directory, and in a directory that does not exist yet.
    for session in ["session.jsonl", "logs/session.jsonl"] {
        let output = zx_run(
            &server,
            &workspace,
            Some(fake.dir.path()),
            &[
                "--execution",
                "container",
                "--container-image",
                "fake:1",
                "--session",
                session,
                "go",
            ],
        )
        .await;

        assert_eq!(output.status.code(), Some(2), "{}", stderr_of(&output));
        assert!(
            stderr_of(&output).contains("needs the session file outside the working directory"),
            "{}",
            stderr_of(&output)
        );
        assert!(!workspace.join(session).exists(), "{session}");
    }
    assert!(server.requests().is_empty());
    assert!(fake.calls().is_empty(), "no container was started");
}

#[tokio::test]
async fn a_container_image_without_container_execution_is_a_configuration_error() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![]).await;

    let output = zx_run(
        &server,
        dir.path(),
        None,
        &["--container-image", "x", "--session", "s.jsonl", "go"],
    )
    .await;

    assert_eq!(output.status.code(), Some(2), "{}", stderr_of(&output));
    assert!(stderr_of(&output).contains("--container-image needs container execution"));
    assert!(server.requests().is_empty());
    assert!(!dir.path().join("s.jsonl").exists());
}

#[tokio::test]
async fn container_execution_without_an_image_fails_before_the_session_is_created() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![]).await;

    let output = zx_run(
        &server,
        dir.path(),
        None,
        &["--execution", "container", "--session", "s.jsonl", "go"],
    )
    .await;

    assert_eq!(output.status.code(), Some(2), "{}", stderr_of(&output));
    assert!(stderr_of(&output).contains("needs container.image"));
    assert!(server.requests().is_empty());
    assert!(!dir.path().join("s.jsonl").exists());
}

#[tokio::test]
async fn a_container_that_cannot_start_stops_the_run_before_any_request() {
    let fake = FakeDocker::new();
    fake.answer("run.fail", "Unable to find image 'fake:1' locally");
    let workspace = fake.workspace();
    let server = MockServer::start(vec![]).await;

    let output = zx_run(
        &server,
        &workspace,
        Some(fake.dir.path()),
        &["--execution", "container", "--container-image", "fake:1", "go"],
    )
    .await;

    assert_eq!(output.status.code(), Some(2), "{}", stderr_of(&output));
    let stderr = stderr_of(&output);
    assert!(stderr.contains("cannot start the container"), "{stderr}");
    assert!(stderr.contains("Unable to find image 'fake:1' locally"), "{stderr}");
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn real_run_in_container_mode_executes_bash_in_the_container() {
    let Some(image) = docker_image() else { return };
    let (_dir, workspace) = real_workspace();
    std::fs::write(workspace.join("app.log"), "public\n").unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let session = sessions.path().join("session.jsonl");
    let server = MockServer::start(vec![Reply::bash("c1", "id -u; cat app.log"), Reply::text("done")]).await;

    let output = zx_run(
        &server,
        &workspace,
        None,
        &[
            "--execution",
            "container",
            "--container-image",
            &image,
            "--tools",
            "bash",
            "--session",
            session.to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    assert!(
        tool_result(&server, 1).starts_with("65534\npublic\n"),
        "{}",
        tool_result(&server, 1)
    );
    let header: Value =
        serde_json::from_str(std::fs::read_to_string(&session).unwrap().lines().next().unwrap()).unwrap();
    let label = format!("label=zx-agent.session={}", header["id"].as_str().unwrap());
    let left = || {
        let output = std::process::Command::new("docker")
            .args(["ps", "--all", "--quiet", "--filter", &label])
            .output()
            .unwrap();
        !output.stdout.is_empty()
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    while left() {
        assert!(Instant::now() < deadline, "the container outlived zx-agent");
        tokio::task::yield_now().await;
    }
}

/// Guards the behavior, not one mechanism: the Docker clients run in their own process group,
/// and on 2026-11-01 an attached `docker run --sig-proxy=false` client also kept running after
/// SIGINT on its own, so this test passes even without the separate group.
#[tokio::test]
async fn real_ctrl_c_in_chat_does_not_stop_the_container() {
    const KEY_VAR: &str = "ZX_P6_CONTAINER_TEST_KEY";
    let Some(image) = docker_image() else { return };
    let (_dir, workspace) = real_workspace();
    let call = json!({"index": 0, "id": "r1", "type": "function",
        "function": {"name": "read_file", "arguments": "{\"path\":\"/etc/os-release\"}"}});
    let server = MockServer::start(vec![
        Reply::Sse(vec![
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [call]}, "finish_reason": "tool_calls"}]}),
        ]),
        Reply::text("done"),
    ])
    .await;
    // Its own process group stands in for a terminal's foreground group.
    let mut chat = Command::new(env!("CARGO_BIN_EXE_zx-agent"))
        .args(["chat", "--model", "mock", "--base-url", &server.base_url])
        .args(["--api-key-env", KEY_VAR, "--max-retries", "0", "--tools", "read_file"])
        .args(["--execution", "container", "--container-image", &image])
        .current_dir(&workspace)
        .env(KEY_VAR, "sk-p6-container-test")
        .env_remove("ZX_MODEL")
        .env_remove("ZX_BASE_URL")
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut errors = BufReader::new(chat.stderr.take().unwrap()).lines();
    let mut wait_for = async |needle: &str| {
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let line = errors.next_line().await.unwrap().expect("chat ended early");
                if line.contains(needle) {
                    break;
                }
            }
        })
        .await
        .unwrap()
    };
    // The banner follows start-up, so the container is running.
    wait_for("[chat] model=").await;
    let group = chat.id().unwrap().to_string();

    // The shell builtin, because procps-ng's kill (4.0.4) rejects `kill -INT -<group>`.
    let signalled = std::process::Command::new("sh")
        .args(["-c", r#"kill -INT -"$1""#, "sh", &group])
        .status()
        .unwrap();
    assert!(signalled.success(), "SIGINT was not sent to group {group}");
    wait_for("no active task").await;
    let mut stdin = chat.stdin.take().unwrap();
    stdin.write_all(b"which system is this?\n/quit\n").await.unwrap();
    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(60), chat.wait())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(status.code(), Some(0));
    let result = tool_result(&server, 1);
    assert!(result.contains("NAME="), "the container was gone: {result}");
}

#[tokio::test]
async fn real_a_killed_zx_agent_leaves_no_container_behind() {
    const KEY_VAR: &str = "ZX_P6_CONTAINER_TEST_KEY";
    let Some(image) = docker_image() else { return };
    let (_dir, workspace) = real_workspace();
    let sessions = tempfile::tempdir().unwrap();
    let session = sessions.path().join("session.jsonl");
    let server = MockServer::start(vec![Reply::bash("c1", "sleep 30")]).await;
    let mut agent = Command::new(env!("CARGO_BIN_EXE_zx-agent"))
        .args(["run", "--model", "mock", "--base-url", &server.base_url])
        .args(["--api-key-env", KEY_VAR, "--max-retries", "0", "--tools", "bash"])
        .args(["--execution", "container", "--container-image", &image])
        .args(["--session", session.to_str().unwrap(), "go"])
        .current_dir(&workspace)
        .env(KEY_VAR, "sk-p6-container-test")
        .env_remove("ZX_MODEL")
        .env_remove("ZX_BASE_URL")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    // The model's request comes after start-up, so the container is running.
    server.wait_for_requests(1).await;
    let header: Value =
        serde_json::from_str(std::fs::read_to_string(&session).unwrap().lines().next().unwrap()).unwrap();
    let label = format!("label=zx-agent.session={}", header["id"].as_str().unwrap());
    let exists = || {
        let output = std::process::Command::new("docker")
            .args(["ps", "--all", "--quiet", "--filter", &label])
            .output()
            .unwrap();
        !output.stdout.is_empty()
    };
    assert!(exists());

    // SIGKILL: zx-agent runs no cleanup; only the closed stdin of its docker client remains.
    agent.kill().await.unwrap();

    let deadline = Instant::now() + Duration::from_secs(20);
    while exists() {
        assert!(Instant::now() < deadline, "the container outlived a killed zx-agent");
        tokio::task::yield_now().await;
    }
}
