//! Submitting a prompt at most once (P10 item 12): `AgentSession::prompt_once` records the
//! key before the prompt and refuses a second submission with the same key.
mod common;

use std::sync::Arc;

use common::{RunnerSetup, config, header, read_lines, summarize};
use zx_agent::AgentSession;
use zx_agent::agent_session::{AgentSessionError, IDEMPOTENCY_KEY_TYPE};
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::runner::RunOutcome;
use zx_agent::session::SessionStore;

fn session_in(file: &std::path::Path, provider: Arc<FauxProvider>) -> AgentSession {
    let (runner, _) = RunnerSetup {
        session: SessionStore::create(file, header()).unwrap(),
        ..RunnerSetup::new(config(provider, vec![]))
    }
    .build();
    AgentSession::from(runner)
}

#[tokio::test]
async fn a_key_is_recorded_before_its_prompt_and_runs_only_once() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("session.jsonl");
    let provider = Arc::new(FauxProvider::new([FauxStep::text("one"), FauxStep::text("two")]));
    let session = session_in(&file, provider.clone());

    let report = session.prompt_once("job-1", "first").await.unwrap();
    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(
        session.prompt_once("job-1", "first again").await.err(),
        Some(AgentSessionError::DuplicateKey)
    );
    assert_eq!(provider.requests().len(), 1, "the duplicate makes no request");
    assert_eq!(session.prompt_once("job-2", "second").await.unwrap().final_text, "two");
    assert_eq!(
        session.prompt_once("  ", "blank").await.err(),
        Some(AgentSessionError::InvalidKey)
    );
    drop(session);

    let lines = read_lines(&file);
    let key = lines
        .iter()
        .position(|line| {
            line["type"] == "custom" && line["customType"] == IDEMPOTENCY_KEY_TYPE && line["data"]["key"] == "job-1"
        })
        .unwrap();
    let prompt = lines
        .iter()
        .position(|line| line["message"]["content"] == "first")
        .unwrap();
    assert!(key < prompt, "the key precedes the prompt it guards");
    assert_eq!(
        summarize(&SessionStore::read(&file).unwrap().messages()),
        ["user:first", "assistant:one", "user:second", "assistant:two"]
    );
}

#[tokio::test]
async fn a_key_survives_reopening_the_session_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("session.jsonl");
    let provider = Arc::new(FauxProvider::new([FauxStep::text("one")]));
    let session = session_in(&file, provider.clone());
    session.prompt_once("job-1", "first").await.unwrap();
    drop(session);

    let (runner, _) = RunnerSetup {
        session: SessionStore::open(&file).unwrap(),
        ..RunnerSetup::new(config(provider.clone(), vec![]))
    }
    .build();
    let reopened = AgentSession::from(runner);

    assert_eq!(
        reopened.prompt_once("job-1", "first").await.err(),
        Some(AgentSessionError::DuplicateKey)
    );
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn an_interrupted_submission_keeps_its_key_and_is_continued_not_repeated() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("session.jsonl");
    let provider = Arc::new(FauxProvider::new([
        FauxStep::Fail("HTTP 500: boom".into()),
        FauxStep::text("done"),
    ]));
    let session = session_in(&file, provider.clone());

    let report = session.prompt_once("job-1", "first").await.unwrap();
    assert_eq!(report.outcome, RunOutcome::Error);
    // The host retries the same submission: refused, so it continues the run instead.
    assert_eq!(
        session.prompt_once("job-1", "first").await.err(),
        Some(AgentSessionError::DuplicateKey)
    );
    let report = session.continue_run().await.unwrap();
    assert_eq!(
        (report.outcome, report.final_text.as_str()),
        (RunOutcome::Completed, "done")
    );
    assert_eq!(provider.requests().len(), 2);
}
