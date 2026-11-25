//! What hooks and a host can do at run time: send the model messages, record state and
//! change the tools the model is offered; and how queued input is taken.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use serde_json::json;
use zx_agent::AgentConfig;
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::{Message, ToolCall, UserMessageKind};
use zx_agent::events::{Event, EventKind, RunOutcome};
use zx_agent::hooks::{Hook, HookContext, HookResult};
use zx_agent::input::{Input, QueueMode};
use zx_agent::session::SessionError;
use zx_agent::tools::ToolOutput;
use zx_agent::transcript::Entry;
use zx_testing::{EchoTool, NotifyingProvider, RunnerSetup, config, header, note, replayed_requests, summarize};

async fn session_in(file: &Path, config: AgentConfig) -> (zx_testing::Driven, Arc<zx_testing::Collect>) {
    RunnerSetup {
        session: zx_testing::create_session_file(file, header()).await.unwrap().into(),
        ..RunnerSetup::new(config)
    }
    .build()
    .await
}

fn kinds(file: &Path) -> Vec<&'static str> {
    zx_testing::read_session_file(file)
        .unwrap()
        .entries()
        .iter()
        .map(Entry::kind)
        .collect()
}

fn tool_names(provider: &FauxProvider, request: usize) -> Vec<String> {
    provider.requests()[request]
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect()
}

/// After each tool result: sends the model a note and records state.
struct Reminder;

#[async_trait]
impl Hook for Reminder {
    async fn after_tool(&self, call: &ToolCall, output: ToolOutput, ctx: &HookContext) -> HookResult<ToolOutput> {
        ctx.queues.steer(Input::Custom(note(&format!("{} ran", call.name))));
        ctx.queues.append_custom_entry("calls", json!({"id": call.id})).unwrap();
        Ok(output)
    }
}

#[tokio::test]
async fn a_hook_sends_the_model_a_message_and_records_state_during_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("session.jsonl");
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c1", "echo", json!({"text": "hi"})),
        FauxStep::text("done"),
    ]));
    let mut config = config(provider.clone(), vec![Arc::new(EchoTool::new("echo"))]);
    config.hooks.push("reminder", Arc::new(Reminder));
    let (session, events) = session_in(&file, config).await;

    let report = session.prompt("go").await.unwrap();

    assert_eq!(report.outcome, RunOutcome::Completed);
    // The next request sees the note as a user message after the tool result.
    assert_eq!(
        summarize(&provider.requests()[1].messages),
        ["user:go", "assistant:calls[c1]", "tool:c1=hi", "user:echo ran"]
    );
    // Both are recorded at the start of the next step, the state before the message.
    let kinds = kinds(&file);
    let state = kinds.iter().position(|kind| *kind == "custom").unwrap();
    assert_eq!(kinds[state + 1], "custom_message", "{kinds:?}");
    let reopened = zx_testing::read_session_file(&file).unwrap();
    assert_eq!(
        summarize(&reopened.messages()),
        [
            "user:go",
            "assistant:calls[c1]",
            "tool:c1=hi",
            "user:echo ran",
            "assistant:done"
        ]
    );
    let events = events.events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            EventKind::MessageEnd {
                message: Message::User { kind: UserMessageKind::Custom, content, .. },
                custom: Some(info),
                ..
            } if content == "echo ran" && info.custom_type == "note" && info.display && info.details == Some(json!({"n": 1}))
        )),
        "{events:?}"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        EventKind::CustomEntry { custom_type, .. } if custom_type == "calls"
    )));
}

/// Records state when the attempt ends: the last events of a run.
struct AtEnd;

#[async_trait]
impl Hook for AtEnd {
    async fn on_event(&self, event: &Event, ctx: &HookContext) -> HookResult<()> {
        if matches!(event.event, EventKind::AttemptEnd { .. }) {
            ctx.queues.append_custom_entry("final", json!(true)).unwrap();
        }
        Ok(())
    }
}

/// When the run starts: queues an entry without a custom type, then one with a type.
struct BlankThenNamed;

#[async_trait]
impl Hook for BlankThenNamed {
    async fn on_event(&self, event: &Event, ctx: &HookContext) -> HookResult<()> {
        if matches!(event.event, EventKind::RunStart { .. }) {
            // Whatever the hook makes of the refusal, the run goes on.
            let _ = ctx.queues.append_custom_entry(" ", json!(null));
            let _ = ctx.queues.append_custom_entry("named", json!(true));
        }
        Ok(())
    }
}

#[tokio::test]
async fn an_entry_without_a_custom_type_is_refused_as_it_is_queued_and_the_run_goes_on() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("session.jsonl");
    let provider = Arc::new(FauxProvider::new([FauxStep::text("done")]));
    let mut config = config(provider, vec![]);
    config.hooks.push("blank_then_named", Arc::new(BlankThenNamed));
    let (session, _) = session_in(&file, config).await;

    let report = session.prompt("go").await.unwrap();

    assert_eq!(report.outcome, RunOutcome::Completed, "{:?}", report.error);
    assert!(kinds(&file).contains(&"custom"), "{:?}", kinds(&file));
}

#[tokio::test]
async fn state_recorded_during_the_last_events_is_kept_before_the_run_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("session.jsonl");
    let provider = Arc::new(FauxProvider::new([FauxStep::text("done")]));
    let mut config = config(provider, vec![]);
    config.hooks.push("at_end", Arc::new(AtEnd));
    let (session, events) = session_in(&file, config).await;

    session.prompt("go").await.unwrap();

    let kinds = kinds(&file);
    assert_eq!(kinds[kinds.len() - 1], "custom", "{kinds:?}");
    let kinds: Vec<&str> = events.kinds();
    let recorded = kinds.iter().position(|kind| *kind == "custom_entry").unwrap();
    assert!(kinds[recorded..].contains(&"run_end"), "{kinds:?}");
}

#[tokio::test]
async fn the_host_limits_the_tools_the_model_is_offered_and_the_audit_records_each_request() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("session.jsonl");
    let note_tool = Arc::new(EchoTool::new("note"));
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c1", "note", json!({"text": "saved"})),
        FauxStep::text("done"),
        FauxStep::text("again"),
    ]));
    let config = config(
        provider.clone(),
        vec![Arc::new(EchoTool::new("echo")), note_tool.clone()],
    );
    let (session, _) = session_in(&file, config).await;

    session.active_tools().set(["echo"]);
    session.prompt("go").await.unwrap();

    assert_eq!(tool_names(&provider, 0), ["echo"]);
    let context = summarize(&provider.requests()[1].messages);
    assert_eq!(
        context[2],
        "tool:c1=Not executed: the tool \"note\" is not active now. Call one of the tools offered.!"
    );
    assert_eq!(note_tool.calls.load(Ordering::SeqCst), 0, "an inactive tool never runs");
    assert_eq!(session.active_tools().names(), Some(vec!["echo".to_string()]));

    session.active_tools().reset();
    session.prompt("more").await.unwrap();

    assert_eq!(tool_names(&provider, 2), ["echo", "note"]);
    let transcript = zx_testing::read_session_file(&file).unwrap();
    let declared: Vec<Vec<String>> = replayed_requests(&transcript)
        .iter()
        .map(|request| request.tools.iter().map(|tool| tool.name.clone()).collect())
        .collect();
    assert_eq!(declared, [vec!["echo"], vec!["echo"], vec!["echo", "note"]]);
}

/// Keeps only `echo` once any tool ran.
struct OnlyEcho;

#[async_trait]
impl Hook for OnlyEcho {
    async fn after_tool(&self, _call: &ToolCall, output: ToolOutput, ctx: &HookContext) -> HookResult<ToolOutput> {
        ctx.active_tools.set(["echo"]);
        Ok(output)
    }
}

#[tokio::test]
async fn a_hook_changes_the_tools_of_the_next_request() {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c1", "echo", json!({"text": "hi"})),
        FauxStep::text("done"),
    ]));
    let mut config = config(
        provider.clone(),
        vec![Arc::new(EchoTool::new("echo")), Arc::new(EchoTool::new("note"))],
    );
    config.hooks.push("only_echo", Arc::new(OnlyEcho));
    let (session, _) = RunnerSetup::new(config).build().await;

    session.prompt("go").await.unwrap();

    assert_eq!(tool_names(&provider, 0), ["echo", "note"]);
    assert_eq!(tool_names(&provider, 1), ["echo"]);
}

#[tokio::test]
async fn state_queued_while_idle_is_recorded_before_the_input_of_the_next_run() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("session.jsonl");
    let provider = Arc::new(FauxProvider::new([FauxStep::text("looking")]));
    let (session, _) = session_in(&file, config(provider.clone(), vec![])).await;

    // What a host does for a command of its own: record what was asked for, then send the
    // model a message, which starts the run.
    session
        .queues()
        .append_custom_entry("jira", json!({"key": "ABC-1"}))
        .unwrap();
    let report = session
        .prompt(Input::Custom(note("Issue ABC-1: broken")))
        .await
        .unwrap();

    assert_eq!(report.final_text, "looking");
    assert_eq!(
        summarize(&provider.requests()[0].messages),
        ["user:Issue ABC-1: broken"]
    );
    // The queued state is written at the run's first step, before its input.
    assert_eq!(&kinds(&file)[..2], ["custom", "custom_message"]);
}

#[tokio::test]
async fn an_idle_session_records_entries_and_messages_at_once_and_a_running_one_is_busy() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("session.jsonl");
    let provider = NotifyingProvider::new(FauxProvider::new([FauxStep::Hang, FauxStep::text("ok")]));
    let (session, _) = session_in(&file, config(provider.clone(), vec![])).await;

    session.append_custom_entry("state", json!({"n": 1})).await.unwrap();
    let id = session.append_custom_message(note("remember")).await.unwrap();
    assert_eq!(kinds(&file), ["custom", "custom_message"]);
    assert_eq!(
        session
            .append_custom_entry(" ", json!(null))
            .await
            .err()
            .map(|error| error.to_string().contains("customType")),
        Some(true)
    );

    let mut running = Box::pin(session.prompt("go"));
    provider.wait_for_start(&mut running).await;
    assert_eq!(
        session.append_custom_entry("state", json!(2)).await,
        Err(SessionError::Busy)
    );
    assert_eq!(
        session.append_custom_message(note("later")).await,
        Err(SessionError::Busy)
    );
    assert!(session.cancel());
    assert_eq!(running.await.unwrap().outcome, RunOutcome::Cancelled);

    // The message recorded while idle is in the context of every later request.
    assert_eq!(
        summarize(&provider.requests()[0].messages),
        ["user:remember", "user:go"]
    );
    let transcript = zx_testing::read_session_file(&file).unwrap();
    assert_eq!(transcript.projection()[0].entry_id, id);
}

/// Steers two messages when the tool runs.
struct TwoSteers;

#[async_trait]
impl Hook for TwoSteers {
    async fn after_tool(&self, _call: &ToolCall, output: ToolOutput, ctx: &HookContext) -> HookResult<ToolOutput> {
        ctx.queues.steer("a");
        ctx.queues.steer("b");
        Ok(output)
    }
}

async fn run_with_mode(mode: QueueMode) -> Vec<Vec<String>> {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c1", "echo", json!({"text": "hi"})),
        FauxStep::text("one"),
        FauxStep::text("two"),
    ]));
    let mut config = config(provider.clone(), vec![Arc::new(EchoTool::new("echo"))]);
    config.hooks.push("two_steers", Arc::new(TwoSteers));
    let (session, _) = RunnerSetup::new(config).build().await;
    session.queues().set_mode(mode);
    session.prompt("go").await.unwrap();
    provider
        .requests()
        .iter()
        .map(|request| summarize(&request.messages))
        .collect()
}

#[tokio::test]
async fn one_at_a_time_gives_each_queued_message_a_step_of_its_own() {
    let all = run_with_mode(QueueMode::All).await;
    assert_eq!(all.len(), 2);
    assert_eq!(&all[1][3..], ["user:a", "user:b"]);

    let one = run_with_mode(QueueMode::OneAtATime).await;
    assert_eq!(one.len(), 3);
    assert_eq!(&one[1][3..], ["user:a"]);
    assert_eq!(&one[2][3..], ["user:a", "assistant:one", "user:b"]);
}
