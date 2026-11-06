//! Recovery from a request the model rejects as too long for its context: with automatic
//! compaction enabled, the runner omits the failed answer, summarizes once and continues.

mod common;

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio_util::sync::CancellationToken;

use zx_agent::agent::{AgentEvent, Tool};
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::{Message, StopReason};
use zx_agent::compaction::Config;
use zx_agent::runner::{Limits, RunOutcome, Runner};
use zx_agent::session::{Entry, SessionStore};

use common::{Collect, EchoTool, RunnerSetup, config, fast_retry, seeded_session, summary_json, test_limits};

const OVERFLOW: &str = "HTTP 400: This model's maximum context length is 65536 tokens";

/// Automatic compaction is enabled, but the context stays below its byte threshold, so only
/// an overflow makes the runner summarize.
fn compaction() -> Config {
    Config {
        trigger_bytes: Some(100_000),
        keep_recent_bytes: 1,
        timeout: Duration::from_secs(1),
        ..Config::default()
    }
}

/// A runner over `session` that asks `provider`, with automatic compaction enabled.
fn runner(provider: &Arc<FauxProvider>, session: SessionStore, limits: Limits) -> (Runner, Arc<Collect>) {
    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(EchoTool::new("echo"))];
    let (runner, events) = RunnerSetup {
        session,
        limits,
        ..RunnerSetup::new(config(provider.clone(), tools))
    }
    .build();
    (runner.with_compaction(compaction()).unwrap(), events)
}

fn count(events: &Collect, kind: &str) -> usize {
    events.kinds().iter().filter(|seen| **seen == kind).count()
}

fn failed_answers(messages: &[Message]) -> usize {
    messages
        .iter()
        .filter(|message| matches!(message, Message::Assistant(answer) if answer.stop_reason == StopReason::Error))
        .count()
}

#[tokio::test]
async fn a_context_the_model_rejects_as_too_long_is_compacted_once_and_the_run_continues() {
    let (session, old) = seeded_session().await;
    let provider = Arc::new(FauxProvider::new([
        FauxStep::Fail(OVERFLOW.into()),
        FauxStep::text(summary_json(&old)),
        FauxStep::text("answer"),
    ]));
    let (mut runner, events) = runner(&provider, session, test_limits());

    let report = runner.run("continue", &CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    assert_eq!(report.final_text, "answer");
    // The summary counts as a request; the recovery is not a retry.
    assert_eq!((report.model_requests, report.retries), (3, 0));
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[1].tools.is_empty(), "the second request is the summary");
    // The new attempt sends the compacted projection: without the failed answer, with the
    // prompt once, and without the summarized message.
    let messages = runner.session().messages();
    assert_eq!(requests[2].messages, messages[..messages.len() - 1]);
    assert_eq!(failed_answers(&requests[2].messages), 0);
    let prompts = requests[2]
        .messages
        .iter()
        .filter(|message| matches!(message, Message::User { content, .. } if content == "continue"))
        .count();
    assert_eq!(prompts, 1);
    assert!(!format!("{:?}", requests[2].messages).contains("observed observed"));
    // The failed answer stays in the session for the audit.
    let recorded = runner
        .session()
        .entries()
        .iter()
        .filter(|entry| matches!(entry, Entry::Message(message) if matches!(&message.message, Message::Assistant(answer) if answer.stop_reason == StopReason::Error)))
        .count();
    assert_eq!(recorded, 1);

    let kinds = events.kinds();
    let overflow = kinds.iter().position(|kind| *kind == "context_overflow").unwrap();
    assert_eq!(
        kinds[overflow..overflow + 5],
        [
            "context_overflow",
            "context_edited",
            "compaction_start",
            "request_start",
            "compaction_end"
        ]
    );
    let attempts: Vec<u32> = events
        .events()
        .iter()
        .filter_map(|event| match event {
            AgentEvent::AgentStart { attempt } => Some(*attempt),
            _ => None,
        })
        .collect();
    assert_eq!(attempts, [1, 2]);
    assert!(
        events
            .events()
            .iter()
            .any(|event| matches!(event, AgentEvent::CompactionStart { automatic: true, .. }))
    );
    assert!(
        events
            .events()
            .contains(&AgentEvent::ContextOverflow { error: OVERFLOW.into() })
    );
}

#[tokio::test]
async fn without_automatic_compaction_an_overflow_ends_the_run_as_before() {
    let (session, _) = seeded_session().await;
    let provider = Arc::new(FauxProvider::new([FauxStep::Fail(OVERFLOW.into())]));
    let (mut runner, events) = RunnerSetup {
        session,
        retry: fast_retry(3),
        ..RunnerSetup::new(config(provider.clone(), vec![]))
    }
    .build();

    let report = runner.run("continue", &CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Error);
    assert_eq!(report.error.as_deref(), Some(OVERFLOW));
    assert_eq!((report.model_requests, report.retries), (1, 0));
    assert_eq!(count(&events, "context_overflow"), 0);
}

#[tokio::test]
async fn an_overflow_right_after_a_recovery_ends_the_run() {
    let (session, old) = seeded_session().await;
    let provider = Arc::new(FauxProvider::new([
        FauxStep::Fail(OVERFLOW.into()),
        FauxStep::text(summary_json(&old)),
        FauxStep::Fail(OVERFLOW.into()),
        FauxStep::text("must not be requested"),
    ]));
    let (mut runner, events) = runner(&provider, session, test_limits());

    let report = runner.run("continue", &CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Error);
    let error = report.error.unwrap_or_default();
    assert!(error.starts_with(OVERFLOW), "{error}");
    assert!(error.contains("still overflowed after compacting it once"), "{error}");
    assert_eq!(provider.requests().len(), 3);
    assert_eq!(count(&events, "context_overflow"), 1);
    assert_eq!(count(&events, "compaction_start"), 1);
}

#[tokio::test]
async fn an_answer_after_a_recovery_allows_another_recovery() {
    let (session, old) = seeded_session().await;
    let provider = Arc::new(FauxProvider::new([
        FauxStep::Fail(OVERFLOW.into()),
        FauxStep::text(summary_json(&old)),
        FauxStep::tool_call("c1", "echo", json!({"text": "hi"})),
        FauxStep::Fail(OVERFLOW.into()),
    ]));
    let (mut runner, events) = runner(&provider, session, test_limits());

    let report = runner.run("continue", &CancellationToken::new()).await;

    // The answer with the tool call succeeded, so the second overflow is recovered from
    // again; there is nothing new before the latest input to summarize, so that fails.
    assert_eq!(report.outcome, RunOutcome::Error);
    assert_eq!(count(&events, "context_overflow"), 2);
    let error = report.error.unwrap_or_default();
    assert!(error.contains("compacting the context to recover failed"), "{error}");
    assert!(!error.contains("still overflowed"), "{error}");
}

#[tokio::test]
async fn a_failed_recovery_summary_pauses_automatic_compaction() {
    let (session, _) = seeded_session().await;
    let provider = Arc::new(FauxProvider::new([
        FauxStep::Fail(OVERFLOW.into()),
        FauxStep::text("not JSON"),
        FauxStep::Fail(OVERFLOW.into()),
        FauxStep::text("must not be requested"),
    ]));
    let (mut runner, _) = runner(&provider, session, test_limits());

    let first = runner.run("continue", &CancellationToken::new()).await;
    assert_eq!(first.outcome, RunOutcome::Error);
    let error = first.error.unwrap_or_default();
    assert!(error.starts_with(OVERFLOW), "{error}");
    assert!(error.contains("compacting the context to recover failed"), "{error}");
    assert_eq!(first.model_requests, 2);
    assert!(matches!(
        runner.session().entries().last(),
        Some(Entry::CompactionFailed(_))
    ));

    let second = runner.run("try again", &CancellationToken::new()).await;
    assert_eq!(second.outcome, RunOutcome::Error);
    assert!(second.error.unwrap_or_default().contains("paused"));
    assert_eq!(provider.requests().len(), 3, "the failed summary is not paid for again");
}

#[tokio::test]
async fn an_overflow_without_request_budget_left_ends_the_run() {
    let (session, _) = seeded_session().await;
    let provider = Arc::new(FauxProvider::new([
        FauxStep::Fail(OVERFLOW.into()),
        FauxStep::text("must not be requested"),
    ]));
    let limits = Limits {
        max_model_requests: Some(1),
        ..test_limits()
    };
    let (mut runner, events) = runner(&provider, session, limits);

    let report = runner.run("continue", &CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Error);
    assert_eq!(report.error.as_deref(), Some(OVERFLOW));
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(count(&events, "compaction_start"), 0);
}

#[tokio::test]
async fn the_run_timeout_also_bounds_the_recovery_summary() {
    let (session, _) = seeded_session().await;
    let provider = Arc::new(FauxProvider::new([FauxStep::Fail(OVERFLOW.into()), FauxStep::Hang]));
    let limits = Limits {
        timeout: Some(Duration::from_millis(200)),
        ..test_limits()
    };
    let (mut runner, _) = runner(&provider, session, limits);

    let report = runner.run("continue", &CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Timeout, "{report:?}");
    assert_eq!(report.model_requests, 2);
    assert!(matches!(
        runner.session().entries().last(),
        Some(Entry::CompactionFailed(_))
    ));
}
