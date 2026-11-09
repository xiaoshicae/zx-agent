//! What extensions can do at run time (P10 item 7): send the model messages, record state,
//! change the tools the model is offered, register commands; and how queued input is taken
//! (item 9).
mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use serde_json::json;

use common::{EchoTool, NotifyingProvider, RunnerSetup, config, header, summarize};
use zx_agent::AgentSession;
use zx_agent::agent::{
    AgentEvent, CommandContext, CommandError, CommandOutcome, Event, HookContext, HookResult, Hooks, LoopConfig,
    QueueMode, QueuedMessage, SlashCommand, ToolOutput,
};
use zx_agent::agent_session::AgentSessionError;
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::{Message, ToolCall, UserMessageKind};
use zx_agent::extensions::{Extension, register};
use zx_agent::runner::RunOutcome;
use zx_agent::session::{CustomMessage, Entry, SessionStore};

fn note(text: &str) -> CustomMessage {
    CustomMessage {
        custom_type: "note".into(),
        content: text.into(),
        display: true,
        details: Some(json!({"n": 1})),
    }
}

fn session_in(file: &Path, config: LoopConfig) -> (AgentSession, Arc<common::Collect>) {
    let (runner, events) = RunnerSetup {
        session: SessionStore::create(file, header()).unwrap(),
        ..RunnerSetup::new(config)
    }
    .build();
    (AgentSession::from(runner), events)
}

fn kinds(file: &Path) -> Vec<&'static str> {
    SessionStore::read(file)
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
impl Hooks for Reminder {
    async fn after_tool(&self, call: &ToolCall, output: ToolOutput, ctx: &HookContext) -> HookResult<ToolOutput> {
        ctx.queues
            .steer_message(QueuedMessage::Custom(note(&format!("{} ran", call.name))));
        ctx.queues.append_entry("calls", json!({"id": call.id}));
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
    config.hooks.push(Reminder);
    let (session, events) = session_in(&file, config);

    let report = session.prompt("go").await.unwrap();

    assert_eq!(report.outcome, RunOutcome::Completed);
    // The next request sees the note as a user message after the tool result.
    assert_eq!(
        summarize(&provider.requests()[1].messages),
        ["user:go", "assistant:calls[c1]", "tool:c1=hi", "user:echo ran"]
    );
    // Both are recorded at the start of the next turn, the state before the message.
    let kinds = kinds(&file);
    let state = kinds.iter().position(|kind| *kind == "custom").unwrap();
    assert_eq!(kinds[state + 1], "custom_message", "{kinds:?}");
    let reopened = SessionStore::read(&file).unwrap();
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
            AgentEvent::MessageEnd {
                message: Message::User { kind: UserMessageKind::Custom, content, .. },
                custom: Some(info),
                ..
            } if content == "echo ran" && info.custom_type == "note" && info.display && info.details == Some(json!({"n": 1}))
        )),
        "{events:?}"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::CustomEntry { custom_type, .. } if custom_type == "calls"
    )));
}

/// Records state when the attempt ends: the last events of a run.
struct AtEnd;

#[async_trait]
impl Hooks for AtEnd {
    async fn on_event(&self, event: &Event, ctx: &HookContext) -> HookResult<()> {
        if matches!(event.event, AgentEvent::AgentEnd { .. }) {
            ctx.queues.append_entry("final", json!(true));
        }
        Ok(())
    }
}

#[tokio::test]
async fn state_recorded_during_the_last_events_is_kept_before_the_run_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("session.jsonl");
    let provider = Arc::new(FauxProvider::new([FauxStep::text("done")]));
    let mut config = config(provider, vec![]);
    config.hooks.push(AtEnd);
    let (session, events) = session_in(&file, config);

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
    let (session, _) = session_in(&file, config);

    session.set_active_tools(&["echo"]).unwrap();
    session.prompt("go").await.unwrap();

    assert_eq!(tool_names(&provider, 0), ["echo"]);
    let context = summarize(&provider.requests()[1].messages);
    assert_eq!(context[2], "tool:c1=Tool note is not active!");
    assert_eq!(note_tool.calls.load(Ordering::SeqCst), 0, "an inactive tool never runs");
    assert_eq!(session.set_active_tools(&["nope"]), Err(AgentSessionError::UnknownTool));
    assert_eq!(session.active_tools().names(), Some(vec!["echo".to_string()]));

    session.active_tools().reset();
    session.prompt("more").await.unwrap();

    assert_eq!(tool_names(&provider, 2), ["echo", "note"]);
    let store = SessionStore::read(&file).unwrap();
    let declared: Vec<Vec<String>> = store
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            Entry::RequestStarted(started) => Some(
                store
                    .replay_request(&started.id)
                    .unwrap()
                    .1
                    .tools
                    .iter()
                    .map(|tool| tool.name.clone())
                    .collect(),
            ),
            _ => None,
        })
        .collect();
    assert_eq!(declared, [vec!["echo"], vec!["echo"], vec!["echo", "note"]]);
}

/// Keeps only `echo` once any tool ran.
struct OnlyEcho;

#[async_trait]
impl Hooks for OnlyEcho {
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
    config.hooks.push(OnlyEcho);
    let (runner, _) = RunnerSetup::new(config).build();
    let session = AgentSession::from(runner);

    session.prompt("go").await.unwrap();

    assert_eq!(tool_names(&provider, 0), ["echo", "note"]);
    assert_eq!(tool_names(&provider, 1), ["echo"]);
}

/// `/jira <key>`: records the key, tells the person, and sends the model the issue.
struct Jira;

#[async_trait]
impl SlashCommand for Jira {
    fn name(&self) -> &str {
        "jira"
    }

    fn description(&self) -> &str {
        "Fetch an issue"
    }

    async fn run(&self, args: &str, ctx: &CommandContext) -> Result<CommandOutcome, String> {
        if args.is_empty() {
            return Err("give an issue key".to_string());
        }
        ctx.queues.append_entry("jira", json!({"key": args}));
        Ok(CommandOutcome {
            lines: vec![format!("fetched {args}")],
            prompt: Some(QueuedMessage::Custom(note(&format!("Issue {args}: broken")))),
        })
    }
}

struct Commands {
    name: &'static str,
    commands: Vec<Arc<dyn SlashCommand>>,
}

impl Extension for Commands {
    fn name(&self) -> &str {
        self.name
    }

    fn commands(&self) -> Vec<Arc<dyn SlashCommand>> {
        self.commands.clone()
    }
}

#[tokio::test]
async fn an_extension_command_runs_through_the_session_and_its_message_starts_a_run() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("session.jsonl");
    let provider = Arc::new(FauxProvider::new([FauxStep::text("looking")]));
    let mut config = config(provider.clone(), vec![]);
    register(
        &mut config,
        &[Arc::new(Commands {
            name: "tracker",
            commands: vec![Arc::new(Jira)],
        })],
    )
    .unwrap();
    let (session, _) = session_in(&file, config);
    assert_eq!(session.commands().len(), 1);
    assert_eq!(session.commands()[0].description(), "Fetch an issue");

    let outcome = session.run_command("jira", "ABC-1").await.unwrap();
    assert_eq!(outcome.lines, ["fetched ABC-1"]);
    let report = session.prompt_message(outcome.prompt.unwrap()).await.unwrap();

    assert_eq!(report.final_text, "looking");
    assert_eq!(
        summarize(&provider.requests()[0].messages),
        ["user:Issue ABC-1: broken"]
    );
    // The state the command recorded is written at the run's first turn, before its input.
    assert_eq!(&kinds(&file)[..2], ["custom", "custom_message"]);
    assert_eq!(session.run_command("nope", "").await, Err(CommandError::Unknown));
    assert_eq!(
        session.run_command("jira", "").await,
        Err(CommandError::Failed("give an issue key".into()))
    );
}

struct Named(&'static str);

#[async_trait]
impl SlashCommand for Named {
    fn name(&self) -> &str {
        self.0
    }

    fn description(&self) -> &str {
        ""
    }

    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandOutcome, String> {
        Ok(CommandOutcome::default())
    }
}

#[test]
fn command_names_must_be_well_formed_and_unique() {
    let cases: Vec<(Vec<&'static str>, &str)> = vec![
        (vec!["jira", "jira"], "already registered"),
        (vec!["my notes"], "must be ASCII"),
        (vec!["1st"], "must be ASCII"),
    ];
    for (names, expected) in cases {
        let mut config = config(Arc::new(FauxProvider::new(Vec::new())), vec![]);
        let extensions: Vec<Arc<dyn Extension>> = names
            .iter()
            .enumerate()
            .map(|(index, name)| {
                Arc::new(Commands {
                    name: ["one", "two"][index],
                    commands: vec![Arc::new(Named(name))],
                }) as Arc<dyn Extension>
            })
            .collect();
        let error = register(&mut config, &extensions).unwrap_err();
        assert!(error.contains(expected), "{error}");
        assert!(config.commands.is_empty(), "nothing is registered");
    }
}

#[tokio::test]
async fn an_idle_session_records_entries_and_messages_at_once_and_a_running_one_is_busy() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("session.jsonl");
    let provider = NotifyingProvider::new([FauxStep::Hang, FauxStep::text("ok")]);
    let (session, _) = session_in(&file, config(provider.clone(), vec![]));

    session.append_entry("state", json!({"n": 1})).await.unwrap();
    let id = session.append_custom_message(note("remember")).await.unwrap();
    assert_eq!(kinds(&file), ["custom", "custom_message"]);
    assert_eq!(
        session
            .append_entry(" ", json!(null))
            .await
            .err()
            .map(|error| error.to_string().contains("customType")),
        Some(true)
    );

    let mut running = Box::pin(session.prompt("go"));
    tokio::select! {
        () = provider.started.notified() => {}
        _ = &mut running => panic!("must be running"),
    }
    assert_eq!(
        session.append_entry("state", json!(2)).await,
        Err(AgentSessionError::Busy)
    );
    assert_eq!(
        session.append_custom_message(note("later")).await,
        Err(AgentSessionError::Busy)
    );
    assert!(session.abort());
    assert_eq!(running.await.unwrap().outcome, RunOutcome::Aborted);

    // The message recorded while idle is in the context of every later request.
    assert_eq!(
        summarize(&provider.requests()[0].messages),
        ["user:remember", "user:go"]
    );
    let store = SessionStore::read(&file).unwrap();
    assert_eq!(store.projection()[0].entry_id, id);
}

/// Steers two messages when the tool runs.
struct TwoSteers;

#[async_trait]
impl Hooks for TwoSteers {
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
    config.hooks.push(TwoSteers);
    config.queues.set_steering_mode(mode);
    let (runner, _) = RunnerSetup::new(config).build();
    let session = AgentSession::from(runner);
    session.prompt("go").await.unwrap();
    provider
        .requests()
        .iter()
        .map(|request| summarize(&request.messages))
        .collect()
}

#[tokio::test]
async fn one_at_a_time_gives_each_queued_message_a_turn_of_its_own() {
    let all = run_with_mode(QueueMode::All).await;
    assert_eq!(all.len(), 2);
    assert_eq!(&all[1][3..], ["user:a", "user:b"]);

    let one = run_with_mode(QueueMode::OneAtATime).await;
    assert_eq!(one.len(), 3);
    assert_eq!(&one[1][3..], ["user:a"]);
    assert_eq!(&one[2][3..], ["user:a", "assistant:one", "user:b"]);
}
