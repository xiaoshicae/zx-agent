//! The executor seam: built-in tools run commands and read files only through their
//! executor, so another executor can run the same tools elsewhere.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use zx_agent::agent::{ExecutionMode, Tool, ToolContext};
use zx_agent::executor::{Captured, CommandEnd, CommandOutput, Executor, ShellCommand};
use zx_agent::tools::{BashTool, ReadFileTool};

/// What a tool asked its executor to do.
#[derive(Debug, PartialEq)]
enum Request {
    Run {
        shell: String,
        command: String,
        cwd: PathBuf,
        timeout: Duration,
        hidden_env: Vec<String>,
    },
    Read {
        path: PathBuf,
        max_bytes: u64,
    },
}

/// Records requests and answers with a scripted command end or file content.
struct Recorder {
    requests: Mutex<Vec<Request>>,
    end: fn() -> CommandEnd,
    file: &'static str,
}

impl Recorder {
    fn new(end: fn() -> CommandEnd) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            end,
            file: "",
        })
    }
}

#[async_trait]
impl Executor for Recorder {
    async fn run(&self, command: &ShellCommand<'_>, _cancel: &CancellationToken) -> Result<CommandOutput, String> {
        self.requests.lock().unwrap().push(Request::Run {
            shell: command.shell.into(),
            command: command.command.into(),
            cwd: command.cwd.into(),
            timeout: command.timeout,
            hidden_env: command.hidden_env.to_vec(),
        });
        Ok(CommandOutput {
            stdout: Captured {
                bytes: b"out".to_vec(),
                dropped: 0,
            },
            stderr: Captured::default(),
            end: (self.end)(),
        })
    }

    async fn read_file(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, String> {
        self.requests.lock().unwrap().push(Request::Read {
            path: path.into(),
            max_bytes,
        });
        Ok(self.file.as_bytes().to_vec())
    }
}

fn ctx() -> ToolContext {
    ToolContext {
        cwd: PathBuf::from("/work"),
        cancel: CancellationToken::new(),
    }
}

#[tokio::test]
async fn bash_hands_the_whole_command_to_its_executor() {
    let executor = Recorder::new(|| CommandEnd::Exited(0));
    let tool = BashTool {
        hidden_env: vec!["KEY".into()],
        executor: executor.clone(),
        ..BashTool::default()
    };

    let output = tool
        .execute(json!({"command": "ls -l", "timeout_secs": 7}), &ctx())
        .await
        .unwrap();

    assert_eq!(output.content, "out\n[exit code: 0]");
    assert_eq!(
        *executor.requests.lock().unwrap(),
        [Request::Run {
            shell: "bash".into(),
            command: "ls -l".into(),
            cwd: "/work".into(),
            timeout: Duration::from_secs(7),
            hidden_env: vec!["KEY".into()],
        }]
    );
}

/// How a command ends, the status line bash reports for it, and whether that is an error.
type EndCase = (fn() -> CommandEnd, &'static str, bool);

#[tokio::test]
async fn bash_reports_every_way_a_command_can_end() {
    let cases: [EndCase; 6] = [
        (|| CommandEnd::Exited(0), "[exit code: 0]", false),
        (|| CommandEnd::Exited(3), "[exit code: 3]", true),
        (|| CommandEnd::Signaled, "[terminated by a signal]", true),
        (
            || CommandEnd::WaitFailed("gone".into()),
            "[failed to wait for the process: gone]",
            true,
        ),
        (
            || CommandEnd::TimedOut,
            "[timed out after 120s; process group killed]",
            true,
        ),
        (|| CommandEnd::Cancelled, "[cancelled; process group killed]", true),
    ];
    for (end, status, is_error) in cases {
        let tool = BashTool {
            executor: Recorder::new(end),
            ..BashTool::default()
        };
        let output = tool.execute(json!({"command": "x"}), &ctx()).await.unwrap();
        assert_eq!(output.content, format!("out\n{status}"));
        assert_eq!(output.is_error, is_error, "{status}");
    }
}

#[tokio::test]
async fn read_file_reads_the_resolved_path_through_its_executor() {
    let executor = Arc::new(Recorder {
        requests: Mutex::new(Vec::new()),
        end: || CommandEnd::Exited(0),
        file: "first\nsecond\n",
    });
    let tool = ReadFileTool {
        max_file_bytes: 64,
        executor: executor.clone(),
    };

    let output = tool
        .execute(json!({"path": "logs/app.log", "offset": 2}), &ctx())
        .await
        .unwrap();

    assert_eq!(output.content, "second\n");
    assert_eq!(
        *executor.requests.lock().unwrap(),
        [Request::Read {
            path: "/work/logs/app.log".into(),
            max_bytes: 64,
        }]
    );
}

/// An executor that handles one call at a time. The test never asks it to do anything.
struct OneAtATime;

#[async_trait]
impl Executor for OneAtATime {
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    async fn run(&self, _command: &ShellCommand<'_>, _cancel: &CancellationToken) -> Result<CommandOutput, String> {
        Err("not used".into())
    }

    async fn read_file(&self, _path: &Path, _max_bytes: u64) -> Result<Vec<u8>, String> {
        Err("not used".into())
    }
}

#[test]
fn tools_declare_the_execution_mode_of_their_executor() {
    // On this machine, and with any executor that does not say otherwise, calls may overlap.
    assert_eq!(BashTool::default().execution_mode(), ExecutionMode::Parallel);
    assert_eq!(ReadFileTool::default().execution_mode(), ExecutionMode::Parallel);

    let executor: Arc<dyn Executor> = Arc::new(OneAtATime);
    let bash = BashTool {
        executor: Arc::clone(&executor),
        ..BashTool::default()
    };
    let read = ReadFileTool {
        executor,
        ..ReadFileTool::default()
    };

    assert_eq!(bash.execution_mode(), ExecutionMode::Sequential);
    assert_eq!(read.execution_mode(), ExecutionMode::Sequential);
}
