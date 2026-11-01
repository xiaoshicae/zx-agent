//! Crash recovery: a call with side effects is recorded as started before it runs, and the
//! next run gives every call that an interrupted run left without a result an explicit one,
//! without running any call again.

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use zx_agent::AgentSession;
use zx_agent::agent::{AgentEvent, Effects, Interruption, Tool, ToolContext, ToolOutput};
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::{AssistantMessage, ContentBlock, Message, StopReason, ToolDef, ToolResultMessage};
use zx_agent::output_store::{self, OutputStore, ReadOutputTool};
use zx_agent::profile::skills::ReadSkillTool;
use zx_agent::runner::RunOutcome;
use zx_agent::session::{SessionHeader, SessionStore};
use zx_agent::tools::builtin_tools;

use common::mock_server::{MockServer, Reply};
use common::{RunnerSetup, config, header, read_lines};

fn definition(name: &str) -> ToolDef {
    ToolDef {
        name: name.into(),
        description: "Test tool".into(),
        parameters: json!({"type": "object"}),
    }
}

/// Reports how many start records the session file held when it ran.
struct StartProbe {
    name: &'static str,
    effects: Effects,
    session: PathBuf,
}

#[async_trait]
impl Tool for StartProbe {
    fn name(&self) -> &str {
        self.name
    }
    fn definition(&self) -> ToolDef {
        definition(self.name)
    }
    fn effects(&self) -> Effects {
        self.effects
    }
    async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, String> {
        let text = std::fs::read_to_string(&self.session).map_err(|error| error.to_string())?;
        let starts = text
            .lines()
            .filter(|line| line.contains(r#""type":"tool_call_started""#))
            .count();
        Ok(ToolOutput::ok(starts.to_string()))
    }
}

fn probe(name: &'static str, effects: Effects, session: &Path) -> Arc<dyn Tool> {
    Arc::new(StartProbe {
        name,
        effects,
        session: session.to_path_buf(),
    })
}

/// Counts its executions; recovery must never run a call again.
struct Counted {
    name: &'static str,
    effects: Effects,
    runs: AtomicUsize,
}

impl Counted {
    fn new(name: &'static str, effects: Effects) -> Arc<Self> {
        Arc::new(Self {
            name,
            effects,
            runs: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl Tool for Counted {
    fn name(&self) -> &str {
        self.name
    }
    fn definition(&self) -> ToolDef {
        definition(self.name)
    }
    fn effects(&self) -> Effects {
        self.effects
    }
    async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, String> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::ok("ran"))
    }
}

/// Signals that it started, then waits until its future is dropped.
struct Hang {
    started: Notify,
    effects: Effects,
}

impl Hang {
    fn new(effects: Effects) -> Arc<Self> {
        Arc::new(Self {
            started: Notify::new(),
            effects,
        })
    }
}

#[async_trait]
impl Tool for Hang {
    fn name(&self) -> &str {
        "hang"
    }
    fn definition(&self) -> ToolDef {
        definition("hang")
    }
    fn effects(&self) -> Effects {
        self.effects
    }
    async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, String> {
        self.started.notify_one();
        std::future::pending().await
    }
}

/// An answer that asked for `calls`, given as `(id, name)`.
fn answer(calls: &[(&str, &str)]) -> Message {
    let mut answer = AssistantMessage::failure("faux", "test-model", StopReason::ToolUse, "");
    answer.error_message = None;
    answer.content = calls
        .iter()
        .map(|(id, name)| ContentBlock::ToolCall {
            id: (*id).into(),
            name: (*name).into(),
            arguments: json!({}),
            arguments_error: None,
        })
        .collect();
    Message::Assistant(answer)
}

fn result(id: &str, name: &str, content: &str) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: id.into(),
        tool_name: name.into(),
        content: content.into(),
        is_error: false,
        timestamp: 0,
        duration_ms: None,
    })
}

/// The tool results in `messages`, as `(tool call id, content, is_error)`.
fn results(messages: &[Message]) -> Vec<(String, String, bool)> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some((result.tool_call_id.clone(), result.content.clone(), result.is_error)),
            Message::System(_) | Message::User { .. } | Message::Assistant(_) => None,
        })
        .collect()
}

fn recovered(events: &[AgentEvent]) -> Vec<(String, Interruption)> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolCallRecovered {
                tool_call_id, state, ..
            } => Some((tool_call_id.clone(), *state)),
            _ => None,
        })
        .collect()
}

#[test]
fn only_bash_among_the_built_in_tools_has_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    let mut tools = builtin_tools(&["bash".into(), "read_file".into()], Duration::from_secs(1), &[]).unwrap();
    let store = OutputStore::new(dir.path(), output_store::Config::default()).unwrap();
    tools.push(Arc::new(ReadOutputTool::new(store)));
    tools.push(Arc::new(ReadSkillTool::new(Vec::new()).unwrap()));
    let effects: Vec<(&str, Effects)> = tools.iter().map(|tool| (tool.name(), tool.effects())).collect();
    assert_eq!(
        effects,
        [
            ("bash", Effects::SideEffects),
            ("read_file", Effects::ReadOnly),
            ("read_output", Effects::ReadOnly),
            ("read_skill", Effects::ReadOnly),
        ]
    );
}

#[tokio::test]
async fn each_start_is_on_disk_before_its_call_runs_and_reads_are_not_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_calls(vec![
            ("c1", "write", json!({})),
            ("c2", "read", json!({})),
            ("c3", "write", json!({})),
        ]),
        FauxStep::text("done"),
    ]));
    let tools = vec![
        probe("write", Effects::SideEffects, &path),
        probe("read", Effects::ReadOnly, &path),
    ];
    let mut config = config(provider.clone(), tools);
    config.spec.sequential_tools = true;
    let (mut runner, _) = RunnerSetup {
        session: SessionStore::create(&path, header()).unwrap(),
        ..RunnerSetup::new(config)
    }
    .build();

    let report = runner.run("go", &CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Completed);
    // Each call saw the starts recorded so far: its own when it has side effects.
    let seen: Vec<String> = results(&provider.requests()[1].messages)
        .into_iter()
        .map(|(_, content, _)| content)
        .collect();
    assert_eq!(seen, ["1", "1", "2"]);
    let starts: Vec<(Value, Value)> = read_lines(&path)
        .into_iter()
        .filter(|line| line["type"] == "tool_call_started")
        .map(|line| (line["toolCallId"].clone(), line["toolName"].clone()))
        .collect();
    assert_eq!(starts, [(json!("c1"), json!("write")), (json!("c3"), json!("write"))]);
}

#[tokio::test]
async fn a_parallel_batch_records_every_start_before_any_call_runs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_calls(vec![
            ("c1", "write", json!({})),
            ("c2", "write", json!({})),
            ("c3", "read", json!({})),
        ]),
        FauxStep::text("done"),
    ]));
    let tools = vec![
        probe("write", Effects::SideEffects, &path),
        probe("read", Effects::ReadOnly, &path),
    ];
    let (mut runner, _) = RunnerSetup {
        session: SessionStore::create(&path, header()).unwrap(),
        ..RunnerSetup::new(config(provider.clone(), tools))
    }
    .build();

    let report = runner.run("go", &CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Completed);
    let seen: Vec<String> = results(&provider.requests()[1].messages)
        .into_iter()
        .map(|(_, content, _)| content)
        .collect();
    assert_eq!(seen, ["2", "2", "2"]);
}

#[tokio::test]
async fn an_interrupted_batch_gets_results_and_no_call_runs_again() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    // What a crash leaves: c1 finished, c2 started, c3 and c4 never ran, c5's tool is gone.
    let mut store = SessionStore::create(&path, header()).unwrap();
    store.append_message(Message::user("fix it")).await.unwrap();
    store
        .append_message(answer(&[
            ("c1", "write"),
            ("c2", "write"),
            ("c3", "write"),
            ("c4", "read"),
            ("c5", "gone"),
        ]))
        .await
        .unwrap();
    store.append_tool_call_started("c1", "write").await.unwrap();
    store.append_tool_call_started("c2", "write").await.unwrap();
    store.append_message(result("c1", "write", "written")).await.unwrap();
    drop(store);
    let write = Counted::new("write", Effects::SideEffects);
    let read = Counted::new("read", Effects::ReadOnly);
    let tools: Vec<Arc<dyn Tool>> = vec![write.clone(), read.clone()];
    let provider = Arc::new(FauxProvider::new([FauxStep::text("checked")]));
    let (mut runner, events) = RunnerSetup {
        session: SessionStore::open(&path).unwrap(),
        ..RunnerSetup::new(config(provider.clone(), tools))
    }
    .build();

    let report = runner.run("continue", &CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(write.runs.load(Ordering::SeqCst) + read.runs.load(Ordering::SeqCst), 0);
    assert_eq!(
        recovered(&events.events()),
        [
            ("c2".into(), Interruption::Started),
            ("c3".into(), Interruption::NotStarted),
            ("c4".into(), Interruption::ReadOnly),
            ("c5".into(), Interruption::StartUnknown),
        ]
    );
    let sent = &provider.requests()[0].messages;
    // Results follow their calls, before the new input.
    let roles: Vec<&str> = sent.iter().map(Message::role).collect();
    assert_eq!(
        roles,
        [
            "user",
            "assistant",
            "toolResult",
            "toolResult",
            "toolResult",
            "toolResult",
            "toolResult",
            "user"
        ]
    );
    let results = results(sent);
    assert_eq!(results[0], ("c1".into(), "written".into(), false));
    let expected = [
        "Result unknown: this call started",
        "Not executed",
        "No result: the agent stopped before this read-only call finished",
        "Result unknown: the agent stopped before this call's result was recorded, and whether the call started was not recorded",
    ];
    for ((_, content, is_error), prefix) in results[1..].iter().zip(expected) {
        assert!(content.starts_with(prefix), "{content}");
        assert!(is_error, "{content}");
    }
    // Recorded in the file, so the next run has nothing to recover.
    assert!(SessionStore::read(&path).unwrap().interrupted_tool_calls().is_empty());
}

#[tokio::test]
async fn calls_of_a_failed_or_cancelled_answer_are_reported_as_not_executed() {
    for stop_reason in [StopReason::Error, StopReason::Aborted] {
        let mut store = SessionStore::in_memory(header());
        store.append_message(Message::user("fix it")).await.unwrap();
        let Message::Assistant(mut failed) = answer(&[("c1", "write"), ("c2", "read")]) else {
            panic!("answer builds an assistant message");
        };
        failed.stop_reason = stop_reason;
        store.append_message(Message::Assistant(failed)).await.unwrap();
        let write = Counted::new("write", Effects::SideEffects);
        let read = Counted::new("read", Effects::ReadOnly);
        let tools: Vec<Arc<dyn Tool>> = vec![write.clone(), read.clone()];
        let provider = Arc::new(FauxProvider::new([FauxStep::text("checked")]));
        let (mut runner, events) = RunnerSetup {
            session: store,
            ..RunnerSetup::new(config(provider.clone(), tools))
        }
        .build();

        let report = runner.run("continue", &CancellationToken::new()).await;

        assert_eq!(report.outcome, RunOutcome::Completed, "{stop_reason:?}");
        assert_eq!(write.runs.load(Ordering::SeqCst) + read.runs.load(Ordering::SeqCst), 0);
        // The loop never runs the calls of such an answer, whatever their effects.
        assert_eq!(
            recovered(&events.events()),
            [
                ("c1".into(), Interruption::NotStarted),
                ("c2".into(), Interruption::NotStarted)
            ],
            "{stop_reason:?}"
        );
        let results = results(&provider.requests()[0].messages);
        assert_eq!(results.len(), 2, "{stop_reason:?}");
        for (_, content, is_error) in &results {
            assert!(content.starts_with("Not executed"), "{content}");
            assert!(is_error, "{content}");
        }
    }
}

#[tokio::test]
async fn the_recovery_event_follows_the_recorded_result() {
    let mut store = SessionStore::in_memory(header());
    store.append_message(Message::user("fix it")).await.unwrap();
    store.append_message(answer(&[("c1", "write")])).await.unwrap();
    let tools: Vec<Arc<dyn Tool>> = vec![Counted::new("write", Effects::SideEffects)];
    let provider = Arc::new(FauxProvider::new([FauxStep::text("checked")]));
    let (mut runner, events) = RunnerSetup {
        session: store,
        ..RunnerSetup::new(config(provider, tools))
    }
    .build();

    runner.run("continue", &CancellationToken::new()).await;

    let events = events.events();
    let recorded = events
        .iter()
        .position(|event| {
            matches!(
                event,
                AgentEvent::MessageEnd {
                    message: Message::ToolResult(_),
                    entry_id: Some(_),
                }
            )
        })
        .unwrap();
    let reported = events
        .iter()
        .position(|event| matches!(event, AgentEvent::ToolCallRecovered { .. }))
        .unwrap();
    assert!(recorded < reported, "{:?}", events);
}

#[tokio::test]
async fn sessions_from_before_start_records_report_an_unknown_start() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v3.jsonl");
    let mut legacy = SessionHeader::new(Path::new("."), "test-model");
    legacy.version = 3;
    let mut store = SessionStore::create(&path, legacy).unwrap();
    store.append_message(Message::user("fix it")).await.unwrap();
    store
        .append_message(answer(&[("c1", "write"), ("c2", "read")]))
        .await
        .unwrap();
    drop(store);
    let write = Counted::new("write", Effects::SideEffects);
    let read = Counted::new("read", Effects::ReadOnly);
    let tools: Vec<Arc<dyn Tool>> = vec![write.clone(), read.clone()];
    let provider = Arc::new(FauxProvider::new([FauxStep::text("checked")]));
    let (mut runner, events) = RunnerSetup {
        session: SessionStore::open(&path).unwrap(),
        ..RunnerSetup::new(config(provider, tools))
    }
    .build();

    let report = runner.run("continue", &CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(
        recovered(&events.events()),
        [
            ("c1".into(), Interruption::StartUnknown),
            ("c2".into(), Interruption::ReadOnly)
        ]
    );
    assert_eq!(write.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_dropped_run_is_recovered_by_the_next_prompt() {
    let hang = Hang::new(Effects::SideEffects);
    let tools: Vec<Arc<dyn Tool>> = vec![hang.clone()];
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c1", "hang", json!({})),
        FauxStep::text("checked"),
    ]));
    let (runner, _) = RunnerSetup::new(config(provider.clone(), tools)).build();
    let session = AgentSession::from(runner);
    {
        let mut running = Box::pin(session.prompt("start"));
        tokio::select! {
            _ = hang.started.notified() => {}
            _ = &mut running => panic!("the run ended before its tool started"),
        }
        // Dropping the run future stops it before the result is recorded.
    }

    let report = session.prompt("again").await.unwrap();

    assert_eq!(report.outcome, RunOutcome::Completed);
    let results = results(&provider.requests()[1].messages);
    assert_eq!(results.len(), 1);
    assert!(
        results[0].1.starts_with("Result unknown: this call started"),
        "{results:?}"
    );
}

/// Cancels a run while its `hang` call runs, then returns the result the next request sends.
async fn result_after_cancelling_a_running_call(effects: Effects) -> (String, bool) {
    let hang = Hang::new(effects);
    let tools: Vec<Arc<dyn Tool>> = vec![hang.clone()];
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c1", "hang", json!({})),
        FauxStep::text("checked"),
    ]));
    let (runner, _) = RunnerSetup::new(config(provider.clone(), tools)).build();
    let session = AgentSession::from(runner);
    let cancel = CancellationToken::new();
    {
        let mut running = Box::pin(session.prompt_with_cancel("start", &cancel));
        tokio::select! {
            _ = hang.started.notified() => {}
            _ = &mut running => panic!("the run ended before its tool started"),
        }
        cancel.cancel();
        assert_eq!(running.await.unwrap().outcome, RunOutcome::Aborted);
    }
    let report = session.prompt("again").await.unwrap();
    assert_eq!(report.outcome, RunOutcome::Completed);
    let results = results(&provider.requests()[1].messages);
    assert_eq!(results.len(), 1, "{results:?}");
    let (_, content, is_error) = results.into_iter().next().unwrap();
    (content, is_error)
}

#[tokio::test]
async fn a_call_with_side_effects_cancelled_while_running_is_reported_as_possibly_applied() {
    let (content, is_error) = result_after_cancelling_a_running_call(Effects::SideEffects).await;
    // The call ran when it was cancelled, so the model must not take it as having done nothing.
    assert!(content.contains("may have changed state"), "{content}");
    assert!(is_error);
}

#[tokio::test]
async fn a_read_only_call_cancelled_while_running_is_reported_as_harmless() {
    let (content, is_error) = result_after_cancelling_a_running_call(Effects::ReadOnly).await;
    assert!(content.contains("changed nothing"), "{content}");
    assert!(is_error);
}

/// `zx-agent run` with bash against the mock server, without retries.
fn run_command(server: &MockServer, dir: &Path, args: &[&str]) -> Command {
    const KEY_VAR: &str = "ZX_P6_RECOVERY_TEST_KEY";
    let mut command = Command::new(env!("CARGO_BIN_EXE_zx-agent"));
    command
        .args(["run", "--model", "mock", "--base-url", &server.base_url])
        .args(["--tools", "bash", "--api-key-env", KEY_VAR, "--max-retries", "0"])
        .args(args)
        .current_dir(dir)
        .env(KEY_VAR, "sk-p6-test")
        .env_remove("ZX_MODEL")
        .env_remove("ZX_BASE_URL")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

/// The tool message that the mock server's request `index` carried.
fn sent_tool_result(server: &MockServer, index: usize) -> Value {
    server.requests()[index].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .cloned()
        .unwrap()
}

#[tokio::test]
async fn run_reports_a_recovered_call_and_does_not_run_the_command_again() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = SessionStore::create(&dir.path().join("session.jsonl"), header()).unwrap();
    store.append_message(Message::user("fix it")).await.unwrap();
    let mut call = AssistantMessage::failure("mock", "mock", StopReason::ToolUse, "");
    call.error_message = None;
    call.content = vec![ContentBlock::ToolCall {
        id: "c1".into(),
        name: "bash".into(),
        arguments: json!({"command": "touch ran-again"}),
        arguments_error: None,
    }];
    store.append_message(Message::Assistant(call)).await.unwrap();
    store.append_tool_call_started("c1", "bash").await.unwrap();
    drop(store);
    let server = MockServer::start(vec![Reply::text("checked")]).await;

    let output = run_command(&server, dir.path(), &["--session", "session.jsonl", "continue"])
        .output()
        .await
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(
        stderr.contains("[recover] bash c1: started, result unknown"),
        "{stderr}"
    );
    assert!(!dir.path().join("ran-again").exists(), "the command ran again");
    let tool = sent_tool_result(&server, 0);
    assert_eq!(tool["tool_call_id"], "c1");
    assert!(
        tool["content"]
            .as_str()
            .unwrap()
            .starts_with("Result unknown: this call started")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_killed_run_is_recovered_by_the_next_run_without_repeating_the_command() {
    let dir = tempfile::tempdir().unwrap();
    // The command logs each execution and records its PID, so the test can stop it later.
    let server = MockServer::start(vec![
        Reply::bash("c1", "echo ran >> runs.log; echo $$ > command.pid; exec sleep 30"),
        Reply::text("checked"),
    ])
    .await;
    let mut first = run_command(&server, dir.path(), &["--session", "session.jsonl", "start"])
        .spawn()
        .unwrap();
    // Once the command runs, its start record has already been written.
    let pid_file = dir.path().join("command.pid");
    let pid = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(text) = tokio::fs::read_to_string(&pid_file).await
                && let Ok(pid) = text.trim().parse::<u32>()
            {
                break pid;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the command never started");
    // SIGKILL: the agent gets no chance to record a result or clean up.
    first.kill().await.unwrap();
    // The command was orphaned with the agent; stop it so it does not outlive the test.
    std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap();

    let output = run_command(&server, dir.path(), &["--session", "session.jsonl", "continue"])
        .output()
        .await
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(
        stderr.contains("[recover] bash c1: started, result unknown"),
        "{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("runs.log")).unwrap(),
        "ran\n",
        "the command ran again"
    );
    let tool = sent_tool_result(&server, 1);
    assert_eq!(tool["tool_call_id"], "c1");
    assert!(
        tool["content"]
            .as_str()
            .unwrap()
            .starts_with("Result unknown: this call started")
    );
}
