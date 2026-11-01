//! Run-level behavior: retries, limits, continuation of cut-off answers, persistence, and
//! resumed sessions.

mod common;

struct CancellationProbe {
    token: std::sync::Mutex<Option<CancellationToken>>,
    started: tokio::sync::Notify,
}

impl zx_agent::ai::Provider for CancellationProbe {
    fn name(&self) -> &str {
        "cancellation-probe"
    }

    fn stream(&self, _request: zx_agent::ai::Request, cancel: CancellationToken) -> zx_agent::ai::EventReceiver {
        *self.token.lock().unwrap() = Some(cancel.clone());
        self.started.notify_one();
        let (mut tx, rx) = zx_agent::ai::provider::event_channel();
        tokio::spawn(async move {
            cancel.cancelled().await;
            let _ = futures::SinkExt::send(
                &mut tx,
                zx_agent::ai::StreamEvent::Error(zx_agent::ai::AssistantMessage::failure(
                    "cancellation-probe",
                    "test-model",
                    StopReason::Aborted,
                    "cancelled",
                )),
            )
            .await;
        });
        rx
    }
}

#[tokio::test]
async fn dropping_a_run_cancels_its_provider() {
    let probe = Arc::new(CancellationProbe {
        token: std::sync::Mutex::new(None),
        started: tokio::sync::Notify::new(),
    });
    let (mut runner, _) = RunnerSetup::new(config(probe.clone(), vec![])).build();
    let cancel = CancellationToken::new();
    let mut running = Box::pin(runner.run("go", &cancel));
    tokio::select! {
        _ = probe.started.notified() => {}
        _ = &mut running => panic!("the provider must still be running"),
    }

    drop(running);
    let cancelled = probe.token.lock().unwrap().as_ref().unwrap().is_cancelled();
    // Clean up the probe even before the production fix, so a failing test leaks no task.
    cancel.cancel();

    assert!(cancelled, "dropping the run left its provider active");
}

use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use common::{EchoTool, RunnerSetup, config, fast_retry, header, read_lines, summarize};
use zx_agent::agent::{AgentEvent, EVENT_SCHEMA_VERSION, HookResult, Hooks, RequestKind};
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::{Message, PREAMBLE, StopReason, SystemPrompt};
use zx_agent::runner::{Limits, RunOutcome};
use zx_agent::session::{Entry, LoadedSession, LogRecord, SessionLog, SessionStore};

/// A stored record, as a database row would hold it.
#[derive(Debug)]
struct Row {
    seq: u64,
    id: String,
    parent_id: Option<String>,
    kind: String,
    json: String,
}

/// Stores records outside a file, like a database table that the test reads back.
struct TableLog(mpsc::Sender<Row>);

#[async_trait]
impl SessionLog for TableLog {
    async fn append(&mut self, record: &LogRecord<'_>) -> anyhow::Result<()> {
        self.0.send(Row {
            seq: record.seq,
            id: record.id.into(),
            parent_id: record.parent_id.map(String::from),
            kind: record.kind.into(),
            json: record.json.into(),
        })?;
        Ok(())
    }
}

#[tokio::test]
async fn a_session_in_a_custom_log_reloads_and_continues_the_same_conversation() {
    let (sender, table) = mpsc::channel();
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c1", "echo", json!({"text": "observed"})),
        FauxStep::text("done"),
    ]));
    let tool = Arc::new(EchoTool::new("echo"));
    let (mut runner, _) = RunnerSetup {
        session: SessionStore::with_log(header(), Box::new(TableLog(sender))),
        ..RunnerSetup::new(config(provider, vec![tool.clone()]))
    }
    .build();

    let report = runner.run("go", &CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    assert_eq!(report.session_file, None);
    let rows: Vec<Row> = table.try_iter().collect();
    let seqs: Vec<u64> = rows.iter().map(|row| row.seq).collect();
    assert_eq!(seqs, (1..=u64::try_from(rows.len()).unwrap()).collect::<Vec<_>>());
    for row in &rows {
        let value: Value = serde_json::from_str(&row.json).unwrap();
        assert_eq!((&value["type"], &value["id"]), (&json!(row.kind), &json!(row.id)));
        assert_eq!(value["parentId"], json!(row.parent_id));
    }
    let kinds: Vec<&str> = rows.iter().map(|row| row.kind.as_str()).collect();
    assert!(kinds.contains(&"tool_call_started"), "{kinds:?}");

    let loaded = LoadedSession {
        header: runner.session().header().clone(),
        records: rows.iter().map(|row| row.json.clone()).collect(),
        repaired_bytes: 0,
    };
    let (sender, more) = mpsc::channel();
    let reloaded = SessionStore::load(loaded, Box::new(TableLog(sender))).unwrap();
    assert_eq!(reloaded.entries(), runner.session().entries());
    let history = runner.session().messages();
    let next = Arc::new(FauxProvider::new([FauxStep::text("again")]));
    let (mut runner, _) = RunnerSetup {
        session: reloaded,
        ..RunnerSetup::new(config(next.clone(), vec![tool]))
    }
    .build();
    let report = runner.run("next", &CancellationToken::new()).await;
    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    let mut expected = history;
    expected.push(Message::user("next"));
    assert_eq!(summarize(&next.requests()[0].messages), summarize(&expected));
    let continued: Vec<u64> = more.try_iter().map(|row| row.seq).collect();
    assert_eq!(continued.first(), Some(&(u64::try_from(rows.len()).unwrap() + 1)));
}

fn provider(steps: impl IntoIterator<Item = FauxStep>) -> Arc<FauxProvider> {
    Arc::new(FauxProvider::new(steps))
}

/// A runner with test defaults around `provider` and no tools.
fn setup(provider: &Arc<FauxProvider>) -> RunnerSetup {
    let mut config = config(provider.clone(), vec![]);
    config.hooks = zx_agent::presets::troubleshoot::hooks();
    RunnerSetup::new(config)
}

fn limits(max_turns: u32, timeout: Option<Duration>) -> Limits {
    Limits {
        max_turns: Some(max_turns),
        timeout,
    }
}

// ---------------------------------------------------------------------------
// Neutral kernel defaults
// ---------------------------------------------------------------------------

#[test]
fn default_limits_have_no_deadline() {
    assert!(Limits::default().timeout.is_none());
}

#[tokio::test]
async fn default_limits_allow_more_than_thirty_requests() {
    let steps = (0..31)
        .map(|index| FauxStep::tool_call(&format!("c{index}"), "echo", json!({"text": "ok"})))
        .chain([FauxStep::text("done")]);
    let provider = provider(steps);
    let (mut runner, _) = RunnerSetup {
        limits: Limits::default(),
        ..RunnerSetup::new(config(provider.clone(), vec![Arc::new(EchoTool::new("echo"))]))
    }
    .build();

    let report = runner.run("go", &CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!((report.turns, report.final_text.as_str()), (32, "done"));
    assert_eq!(provider.requests().len(), 32);
}

#[tokio::test]
async fn default_limits_leave_truncated_answers_for_the_caller() {
    let provider = provider([FauxStep::truncated("part one"), FauxStep::text("part two")]);
    let (mut runner, _) = RunnerSetup {
        limits: Limits::default(),
        ..RunnerSetup::new(config(provider.clone(), vec![]))
    }
    .build();

    let report = runner.run("go", &CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Truncated);
    assert_eq!((report.turns, report.final_text.as_str()), (1, "part one"));
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn zero_turn_limit_does_not_call_the_provider() {
    let provider = provider([FauxStep::text("must not be requested")]);
    let (mut runner, _) = RunnerSetup {
        limits: limits(0, None),
        ..setup(&provider)
    }
    .build();

    let report = runner.run("go", &CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::MaxTurns);
    assert_eq!(report.turns, 0);
    assert!(report.final_text.is_empty());
    assert!(provider.requests().is_empty());
    assert_eq!(report.error.as_deref(), Some("turn limit reached (max_turns = 0)"));
}

#[tokio::test]
async fn an_unlimited_budget_can_explicitly_enable_continuation() {
    let provider = provider([FauxStep::truncated("part one, "), FauxStep::text("part two")]);
    let (mut runner, _) = RunnerSetup {
        limits: Limits::default(),
        ..setup(&provider)
    }
    .build();

    let report = runner.run("go", &CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!((report.turns, report.final_text.as_str()), (2, "part one, part two"));
}

// ---------------------------------------------------------------------------
// Retries
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retries_transient_errors_without_repeating_the_prompt() {
    let provider = provider([
        FauxStep::Fail("HTTP 503: service unavailable".into()),
        FauxStep::text("ok"),
    ]);
    let (mut runner, sink) = RunnerSetup {
        retry: fast_retry(3),
        limits: Limits::default(),
        ..setup(&provider)
    }
    .build();

    let report = runner.run("diagnose", &CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!((report.retries, report.turns, report.final_text.as_str()), (1, 2, "ok"));
    // The retry request has the prompt once and not the failed attempt.
    assert_eq!(summarize(&provider.requests()[1].messages), ["user:diagnose"]);
    assert!(sink.kinds().contains(&"retry_scheduled"));
}

#[tokio::test]
async fn a_retried_failure_stays_in_the_file_but_leaves_the_context() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("run.jsonl");
    let provider = provider([
        FauxStep::Fail("HTTP 503: service unavailable".into()),
        FauxStep::text("ok"),
    ]);
    let (mut runner, _) = RunnerSetup {
        retry: fast_retry(3),
        session: SessionStore::create(&path, header()).unwrap(),
        ..setup(&provider)
    }
    .build();

    runner.run("diagnose", &CancellationToken::new()).await;

    let lines = read_lines(&path);
    let types: Vec<&str> = lines.iter().map(|line| line["type"].as_str().unwrap()).collect();
    // Each dispatch is checkpointed first; the unchanged configuration is recorded once.
    assert_eq!(
        types,
        [
            "session",
            "message",
            "request_config",
            "request_started",
            "message",
            "context_edit",
            "request_started",
            "message"
        ]
    );
    assert_eq!(lines[4]["message"]["stopReason"], "error");
    assert_eq!(lines[5]["targetId"], lines[4]["id"]);
    assert_eq!(lines[5]["replacement"], Value::Null);
    assert_eq!(
        summarize(&runner.session().messages()),
        ["user:diagnose", "assistant:ok"]
    );
}

#[tokio::test]
async fn permanent_errors_are_not_retried() {
    let provider = provider([FauxStep::Fail("HTTP 401: invalid api key".into())]);
    let (mut runner, _) = RunnerSetup {
        retry: fast_retry(3),
        ..setup(&provider)
    }
    .build();

    let report = runner.run("go", &CancellationToken::new()).await;

    assert_eq!((report.outcome, report.retries), (RunOutcome::Error, 0));
    assert!(report.error.unwrap_or_default().contains("401"));
}

#[tokio::test]
async fn retrying_stops_after_max_retries() {
    let provider = provider((0..5).map(|_| FauxStep::Fail("HTTP 502: bad gateway".into())));
    let (mut runner, _) = RunnerSetup {
        retry: fast_retry(2),
        ..setup(&provider)
    }
    .build();

    let report = runner.run("go", &CancellationToken::new()).await;

    assert_eq!((report.outcome, report.retries), (RunOutcome::Error, 2));
    assert_eq!(provider.requests().len(), 3);
}

// ---------------------------------------------------------------------------
// Limits and cancellation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_run_stops_at_the_turn_limit() {
    let provider = provider((0..10).map(|i| FauxStep::tool_call(&format!("c{i}"), "echo", json!({"text": "loop"}))));
    let (mut runner, _) = RunnerSetup {
        limits: limits(3, None),
        ..RunnerSetup::new(config(provider.clone(), vec![Arc::new(EchoTool::new("echo"))]))
    }
    .build();

    let report = runner.run("go", &CancellationToken::new()).await;

    assert_eq!((report.outcome, report.turns), (RunOutcome::MaxTurns, 3));
}

#[tokio::test]
async fn a_run_stops_at_the_time_limit() {
    let provider = provider([FauxStep::Hang]);
    let (mut runner, _) = RunnerSetup {
        limits: limits(10, Some(Duration::from_millis(150))),
        ..setup(&provider)
    }
    .build();

    let report = runner.run("go", &CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Timeout);
}

#[tokio::test]
async fn the_caller_can_cancel_a_waiting_run() {
    let provider = provider([FauxStep::Hang]);
    let (mut runner, _) = RunnerSetup {
        limits: Limits::default(),
        ..setup(&provider)
    }
    .build();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    // The delay only lets the request start; the assertion does not depend on timing.
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        trigger.cancel();
    });

    let report = runner.run("go", &cancel).await;

    assert_eq!(report.outcome, RunOutcome::Aborted);
}

// ---------------------------------------------------------------------------
// Answers cut off at the output token limit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn continues_a_truncated_answer_and_joins_the_parts() {
    let provider = provider([FauxStep::truncated("Part one, "), FauxStep::text("part two.")]);
    let (mut runner, _) = setup(&provider).build();

    let report = runner.run("go", &CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!((report.turns, report.final_text.as_str()), (2, "Part one, part two."));
    let second = summarize(&provider.requests()[1].messages);
    assert_eq!(second[..2], ["user:go", "assistant:Part one, "]);
    assert!(
        second[2].starts_with("user:Your previous response was cut off"),
        "{second:?}"
    );
}

#[tokio::test]
async fn a_continuation_that_repeats_the_cut_line_is_joined_once() {
    let provider = provider([
        FauxStep::truncated("Steps:\n1. Check logs\n2. Add a pre-deploy"),
        FauxStep::text("2. Add a pre-deploy check."),
    ]);
    let (mut runner, _) = setup(&provider).build();

    let report = runner.run("go", &CancellationToken::new()).await;

    assert_eq!(report.final_text, "Steps:\n1. Check logs\n2. Add a pre-deploy check.");
}

#[tokio::test]
async fn reports_truncated_when_no_continuation_is_allowed() {
    let provider = provider([FauxStep::truncated("Part one")]);
    let (mut runner, _) = RunnerSetup {
        limits: limits(30, None),
        ..RunnerSetup::new(config(provider.clone(), vec![]))
    }
    .build();

    let report = runner.run("go", &CancellationToken::new()).await;

    assert_eq!(
        (report.outcome, report.final_text.as_str()),
        (RunOutcome::Truncated, "Part one")
    );
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn continuations_count_against_the_turn_limit() {
    let provider = provider([FauxStep::truncated("a"), FauxStep::text("b")]);
    let (mut runner, _) = RunnerSetup {
        limits: limits(1, None),
        ..setup(&provider)
    }
    .build();

    let report = runner.run("go", &CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Truncated);
    assert_eq!(provider.requests().len(), 1);
}

// ---------------------------------------------------------------------------
// Persistence and resumed sessions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn events_are_forwarded_after_persistence() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.jsonl");
    let provider = provider([FauxStep::text("hi")]);
    let (mut runner, sink) = RunnerSetup {
        session: SessionStore::create(&path, header()).unwrap(),
        ..setup(&provider)
    }
    .build();

    runner.run("hello", &CancellationToken::new()).await;

    let ends = sink
        .events()
        .into_iter()
        .filter(|event| matches!(event, AgentEvent::MessageEnd { .. }))
        .count();
    // One message line per finished message; request checkpoints are separate entries.
    let messages = read_lines(&path)
        .iter()
        .filter(|line| line["type"] == "message")
        .count();
    assert_eq!(messages, ends);
}

#[tokio::test]
async fn final_text_ignores_answers_from_earlier_runs() {
    let provider = provider([FauxStep::text("old answer")]);
    let (mut runner, _) = setup(&provider).build();
    runner.run("first", &CancellationToken::new()).await;

    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let report = runner.run("second", &cancelled).await;

    assert_eq!((report.outcome, report.final_text.as_str()), (RunOutcome::Aborted, ""));
}

#[tokio::test]
async fn resumed_sessions_continue_the_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let first = provider([FauxStep::text("nginx is down")]);
    let (mut runner, _) = RunnerSetup {
        session: SessionStore::create(&path, header()).unwrap(),
        ..setup(&first)
    }
    .build();
    runner.run("check nginx", &CancellationToken::new()).await;
    drop(runner);

    let second = provider([FauxStep::text("restart it")]);
    let (mut runner, _) = RunnerSetup {
        session: SessionStore::open(&path).unwrap(),
        ..setup(&second)
    }
    .build();
    let report = runner.run("what now?", &CancellationToken::new()).await;

    assert_eq!(report.final_text, "restart it");
    assert_eq!(
        summarize(&second.requests()[0].messages),
        ["user:check nginx", "assistant:nginx is down", "user:what now?"]
    );
}

/// One run on `session` whose environment section says `started`.
async fn run_with_environment(session: SessionStore, started: &str) {
    let mut config = config(provider([FauxStep::text("ok")]), vec![]);
    config.spec.system_prompt = SystemPrompt::default()
        .with_section(PREAMBLE, "Do it.")
        .with_section("environment", started);
    let (mut runner, _) = RunnerSetup {
        session,
        ..RunnerSetup::new(config)
    }
    .build();
    runner.run("go", &CancellationToken::new()).await;
}

#[tokio::test]
async fn resumed_sessions_record_prompt_changes_in_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.jsonl");
    run_with_environment(SessionStore::create(&path, header()).unwrap(), "t1").await;
    run_with_environment(SessionStore::open(&path).unwrap(), "t2").await;

    // Request checkpoints sit between messages; only message lines carry roles.
    let messages: Vec<Value> = read_lines(&path)
        .into_iter()
        .filter(|line| line["type"] == "message")
        .collect();
    let roles: Vec<&str> = messages
        .iter()
        .map(|line| line["message"]["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["system", "user", "assistant", "system", "user", "assistant"]);
    assert_eq!(
        messages[3]["message"]["sections"],
        json!([{"name": "environment", "content": "t2"}])
    );
}

/// A run that fails once with a retryable error, then answers.
fn retried_run() -> (zx_agent::runner::Runner, Arc<common::Collect>) {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::Fail("HTTP 503: retry".into()),
        FauxStep::text("done"),
    ]));
    RunnerSetup {
        retry: fast_retry(1),
        ..RunnerSetup::new(config(provider, vec![]))
    }
    .build()
}

#[tokio::test]
async fn every_event_of_a_run_carries_its_ids_and_the_next_sequence_number() {
    let (mut runner, events) = retried_run();

    runner.run("go", &CancellationToken::new()).await;

    let envelopes = events.envelopes();
    let seqs: Vec<u64> = envelopes.iter().map(|event| event.seq).collect();
    assert_eq!(seqs, (1..=u64::try_from(envelopes.len()).unwrap()).collect::<Vec<_>>());
    let session_id = runner.session().header().id.clone();
    for event in &envelopes {
        assert_eq!(event.schema_version, EVENT_SCHEMA_VERSION);
        assert_eq!(event.run_id, envelopes[0].run_id);
        assert_eq!(event.session_id.as_deref(), Some(session_id.as_str()));
    }
    let first = envelopes.len();
    runner.run("again", &CancellationToken::new()).await;
    let next = &events.envelopes()[first..];
    assert_ne!(next[0].run_id, envelopes[0].run_id, "each run has its own ID");
    assert_eq!(next[0].seq, 1);
}

#[tokio::test]
async fn a_run_starts_with_run_start_and_ends_with_its_report() {
    let (mut runner, events) = retried_run();

    let report = runner.run("go", &CancellationToken::new()).await;

    let all = events.events();
    assert!(matches!(&all[0], AgentEvent::RunStart { model, .. } if model == "test-model"));
    match all.last() {
        Some(AgentEvent::RunEnd { report: ended }) => assert_eq!(**ended, report),
        other => panic!("the last event is not run_end: {other:?}"),
    }
    assert_eq!(
        all.iter()
            .filter(|event| matches!(event, AgentEvent::RunEnd { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn attempts_and_requests_are_numbered_and_name_their_request_records() {
    let (mut runner, events) = retried_run();

    runner.run("go", &CancellationToken::new()).await;

    let all = events.events();
    let attempts: Vec<u32> = all
        .iter()
        .filter_map(|event| match event {
            AgentEvent::AgentStart { attempt } => Some(*attempt),
            _ => None,
        })
        .collect();
    assert_eq!(attempts, [1, 2]);
    let requests: Vec<(RequestKind, u32, Option<String>)> = all
        .iter()
        .filter_map(|event| match event {
            AgentEvent::RequestStart {
                kind,
                attempt,
                entry_id,
                ..
            } => Some((*kind, *attempt, entry_id.clone())),
            _ => None,
        })
        .collect();
    let recorded: Vec<Option<String>> = runner
        .session()
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            Entry::RequestStarted(started) => Some(Some(started.id.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        requests,
        [
            (RequestKind::Task, 1, recorded[0].clone()),
            (RequestKind::Task, 2, recorded[1].clone())
        ]
    );
    let turns: Vec<u32> = all
        .iter()
        .filter_map(|event| match event {
            AgentEvent::TurnStart { turn } | AgentEvent::TurnEnd { turn } => Some(*turn),
            _ => None,
        })
        .collect();
    assert_eq!(turns, [1, 1, 2, 2], "turn numbers continue across attempts");
}

struct Uppercase;
#[async_trait]
impl Hooks for Uppercase {
    async fn on_input(&self, text: String) -> HookResult<String> {
        Ok(text.to_uppercase())
    }
}

#[tokio::test]
async fn messages_and_context_edits_name_their_session_entries() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("done")]));
    let mut config = config(provider, vec![]);
    config.hooks.push(Uppercase);
    let (mut runner, events) = RunnerSetup::new(config).build();

    runner.run("go", &CancellationToken::new()).await;

    let reported: Vec<(&'static str, Option<String>)> = events
        .events()
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::MessageEnd { entry_id, .. } => Some(("message", entry_id)),
            AgentEvent::ContextEdited { entry_id, .. } => Some(("context_edit", entry_id)),
            _ => None,
        })
        .collect();
    let recorded: Vec<(&'static str, Option<String>)> = runner
        .session()
        .entries()
        .iter()
        .filter(|entry| matches!(entry, Entry::Message(_) | Entry::ContextEdit(_)))
        .map(|entry| (entry.kind(), Some(entry.id().to_string())))
        .collect();
    assert_eq!(reported, recorded);
    assert_eq!(reported.len(), 3, "the input, its rewrite and the answer");
}
