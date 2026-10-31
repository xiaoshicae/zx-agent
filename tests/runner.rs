//! Run-level behavior: retries, limits, continuation of cut-off answers, persistence, and
//! resumed sessions.

mod common;

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use common::{EchoTool, RunnerSetup, config, fast_retry, header, read_lines, summarize};
use zx_agent::agent::AgentEvent;
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::{PREAMBLE, SystemPrompt};
use zx_agent::runner::{Limits, RunOutcome};
use zx_agent::session::SessionStore;

fn provider(steps: impl IntoIterator<Item = FauxStep>) -> Arc<FauxProvider> {
    Arc::new(FauxProvider::new(steps))
}

/// A runner with test defaults around `provider` and no tools.
fn setup(provider: &Arc<FauxProvider>) -> RunnerSetup {
    RunnerSetup::new(config(provider.clone(), vec![]))
}

fn limits(max_turns: u32, timeout: Option<Duration>, continue_on_length: u32) -> Limits {
    Limits {
        max_turns,
        timeout,
        continue_on_length,
    }
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
    let (runner, sink) = RunnerSetup {
        retry: fast_retry(3),
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
    let (runner, _) = RunnerSetup {
        retry: fast_retry(3),
        session: SessionStore::create(&path, header()).unwrap(),
        ..setup(&provider)
    }
    .build();

    runner.run("diagnose", &CancellationToken::new()).await;

    let lines = read_lines(&path);
    let types: Vec<&str> = lines.iter().map(|line| line["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["session", "message", "message", "context_edit", "message"]);
    assert_eq!(lines[2]["message"]["stopReason"], "error");
    assert_eq!(lines[3]["targetId"], lines[2]["id"]);
    assert_eq!(lines[3]["replacement"], Value::Null);
    assert_eq!(
        summarize(&runner.session().messages()),
        ["user:diagnose", "assistant:ok"]
    );
}

#[tokio::test]
async fn permanent_errors_are_not_retried() {
    let provider = provider([FauxStep::Fail("HTTP 401: invalid api key".into())]);
    let (runner, _) = RunnerSetup {
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
    let (runner, _) = RunnerSetup {
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
    let (runner, _) = RunnerSetup {
        limits: limits(3, None, 0),
        ..RunnerSetup::new(config(provider.clone(), vec![Arc::new(EchoTool::new("echo"))]))
    }
    .build();

    let report = runner.run("go", &CancellationToken::new()).await;

    assert_eq!((report.outcome, report.turns), (RunOutcome::MaxTurns, 3));
}

#[tokio::test]
async fn a_run_stops_at_the_time_limit() {
    let provider = provider([FauxStep::Hang]);
    let (runner, _) = RunnerSetup {
        limits: limits(10, Some(Duration::from_millis(150)), 0),
        ..setup(&provider)
    }
    .build();

    let report = runner.run("go", &CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Timeout);
}

#[tokio::test]
async fn the_caller_can_cancel_a_waiting_run() {
    let provider = provider([FauxStep::Hang]);
    let (runner, _) = setup(&provider).build();
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
    let (runner, _) = setup(&provider).build();

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
    let (runner, _) = setup(&provider).build();

    let report = runner.run("go", &CancellationToken::new()).await;

    assert_eq!(report.final_text, "Steps:\n1. Check logs\n2. Add a pre-deploy check.");
}

#[tokio::test]
async fn reports_truncated_when_no_continuation_is_allowed() {
    let provider = provider([FauxStep::truncated("Part one")]);
    let (runner, _) = RunnerSetup {
        limits: limits(30, None, 0),
        ..setup(&provider)
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
    let (runner, _) = RunnerSetup {
        limits: limits(1, None, 1),
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
    let (runner, sink) = RunnerSetup {
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
    // Header plus one line per finished message.
    assert_eq!(read_lines(&path).len(), 1 + ends);
}

#[tokio::test]
async fn final_text_ignores_answers_from_earlier_runs() {
    let provider = provider([FauxStep::text("old answer")]);
    let (runner, _) = setup(&provider).build();
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
    let (runner, _) = RunnerSetup {
        session: SessionStore::create(&path, header()).unwrap(),
        ..setup(&first)
    }
    .build();
    runner.run("check nginx", &CancellationToken::new()).await;
    drop(runner);

    let second = provider([FauxStep::text("restart it")]);
    let (runner, _) = RunnerSetup {
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
    config.system_prompt = SystemPrompt::default()
        .with_section(PREAMBLE, "Do it.")
        .with_section("environment", started);
    let (runner, _) = RunnerSetup {
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

    let lines = read_lines(&path);
    let roles: Vec<&str> = lines[1..]
        .iter()
        .map(|line| line["message"]["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["system", "user", "assistant", "system", "user", "assistant"]);
    assert_eq!(
        lines[4]["message"]["sections"],
        json!([{"name": "environment", "content": "t2"}])
    );
}
