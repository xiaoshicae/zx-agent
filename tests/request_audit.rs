//! Request audit (G3): every model request can be rebuilt from the session file.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio_util::sync::CancellationToken;

use zx_agent::AgentSession;
use zx_agent::agent::Tool;
use zx_agent::agent_session::ModelRef;
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::{Message, PREAMBLE, Request, SystemPrompt};
use zx_agent::compaction::{Config, Outcome};
use zx_agent::runner::RunOutcome;
use zx_agent::session::{Entry, RequestSource, SessionHeader, SessionStore};

use common::{EchoTool, RunnerSetup, config, header, read_lines};

/// Request checkpoint IDs on the session's entry list, in file order.
fn request_ids(store: &SessionStore) -> Vec<String> {
    store
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            Entry::RequestStarted(started) => Some(started.id.clone()),
            _ => None,
        })
        .collect()
}

/// Every request rebuilt from a freshly opened copy of the file.
fn replayed(path: &Path) -> Vec<Request> {
    let store = SessionStore::open(path).unwrap();
    request_ids(&store)
        .iter()
        .map(|id| store.replay_request(id).unwrap().1)
        .collect()
}

fn types(path: &Path) -> Vec<String> {
    read_lines(path)
        .iter()
        .map(|line| line["type"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn every_dispatched_request_is_rebuilt_exactly_from_the_session_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c1", "echo", json!({"text": "observed"})),
        FauxStep::text("done"),
    ]));
    let tool: Arc<dyn Tool> = Arc::new(EchoTool::new("echo"));
    let mut config = config(provider.clone(), vec![tool]);
    config.spec.system_prompt = SystemPrompt::default().with_section(PREAMBLE, "Be brief.");
    config.spec.max_tokens = Some(256);
    let (mut runner, _) = RunnerSetup {
        session: SessionStore::create(&path, header()).unwrap(),
        ..RunnerSetup::new(config)
    }
    .build();

    let report = runner.run("inspect", &CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Completed);
    let sent = provider.requests();
    assert_eq!(sent.len(), 2);
    // The second request carries the tool result, so it differs from the first.
    assert_ne!(sent[0].messages, sent[1].messages);
    assert_eq!(replayed(&path), sent);
}

#[tokio::test]
async fn request_config_is_appended_only_when_the_configuration_changes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let first = Arc::new(FauxProvider::new([FauxStep::text("one"), FauxStep::text("two")]));
    let second = Arc::new(FauxProvider::new([FauxStep::text("three")]));
    let (runner, _) = RunnerSetup {
        session: SessionStore::create(&path, header()).unwrap(),
        ..RunnerSetup::new(config(first.clone(), vec![]))
    }
    .build();
    let session = AgentSession::from(runner);

    session.prompt("a").await.unwrap();
    session.prompt("b").await.unwrap();
    session
        .set_model(ModelRef {
            provider: second.clone(),
            model: "other-model".into(),
        })
        .unwrap();
    session.prompt("c").await.unwrap();

    let configs = types(&path).iter().filter(|kind| *kind == "request_config").count();
    assert_eq!(configs, 2, "{:?}", types(&path));
    let mut sent = first.requests();
    sent.extend(second.requests());
    let rebuilt = replayed(&path);
    assert_eq!(rebuilt, sent);
    assert_eq!(rebuilt[2].model, "other-model");
}

#[tokio::test]
async fn legacy_sessions_upgrade_once_before_the_first_request() {
    let dir = tempfile::tempdir().unwrap();
    for version in [1, 2] {
        let path = dir.path().join(format!("v{version}.jsonl"));
        let mut legacy = SessionHeader::new(Path::new("."), "test-model");
        legacy.version = version;
        let mut store = SessionStore::create(&path, legacy).unwrap();
        store.append_message(Message::user("earlier")).unwrap();
        drop(store);
        let before = std::fs::read(&path).unwrap();

        for answer in ["one", "two"] {
            let provider = Arc::new(FauxProvider::new([FauxStep::text(answer)]));
            let (mut runner, _) = RunnerSetup {
                session: SessionStore::open(&path).unwrap(),
                ..RunnerSetup::new(config(provider, vec![]))
            }
            .build();
            let report = runner.run("go", &CancellationToken::new()).await;
            assert_eq!(report.outcome, RunOutcome::Completed);
        }

        assert!(std::fs::read(&path).unwrap().starts_with(&before));
        let kinds = types(&path);
        assert_eq!(kinds.iter().filter(|kind| *kind == "format_upgrade").count(), 1);
        let upgrade = kinds.iter().position(|kind| kind == "format_upgrade").unwrap();
        let config = kinds.iter().position(|kind| kind == "request_config").unwrap();
        assert!(upgrade < config, "v{version}: {kinds:?}");
        let reopened = SessionStore::open(&path).unwrap();
        assert_eq!(reopened.header().version, version);
        assert_eq!(reopened.format_version(), 3);
        assert_eq!(replayed(&path).len(), 2);
    }
}

#[tokio::test]
async fn summary_requests_are_rebuilt_from_the_compaction_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let mut store = SessionStore::create(&path, header()).unwrap();
    let old = store.append_message(Message::user("observed ".repeat(600))).unwrap();
    store.append_message(Message::user("most recent input")).unwrap();
    let summary =
        json!({"facts":[{"text":"Old observation","sourceEntryIds":[old]}],"inferences":[],"plans":[]}).to_string();
    let provider = Arc::new(FauxProvider::new([FauxStep::text(summary)]));
    let (runner, _) = RunnerSetup {
        session: store,
        ..RunnerSetup::new(config(provider.clone(), vec![]))
    }
    .build();
    let session = AgentSession::from(
        runner
            .with_compaction(Config {
                keep_recent_bytes: 1,
                timeout: Duration::from_secs(1),
                ..Config::default()
            })
            .unwrap(),
    );

    let report = session.compact().await.unwrap();

    assert_eq!(report.outcome, Outcome::Completed, "{report:?}");
    let reopened = SessionStore::open(&path).unwrap();
    let ids = request_ids(&reopened);
    assert_eq!(ids.len(), 1);
    let Some(Entry::RequestStarted(started)) = reopened.get(&ids[0]) else {
        panic!("not a request checkpoint");
    };
    assert!(matches!(started.source, RequestSource::Summary { .. }));
    let (config, request) = reopened.replay_request(&ids[0]).unwrap();
    assert!(config.tools.is_empty());
    assert_eq!(vec![request], provider.requests());
}

#[test]
fn only_request_checkpoints_can_be_replayed() {
    let mut store = SessionStore::in_memory(header());
    let message = store.append_message(Message::user("not a request")).unwrap();
    assert!(store.replay_request(&message).is_err());
    assert!(store.replay_request("missing").is_err());
}

#[test]
fn a_request_that_differs_from_the_persisted_projection_is_not_recorded() {
    let mut store = SessionStore::in_memory(header());
    store.append_message(Message::user("persisted")).unwrap();
    let before = store.entries().len();
    let request = Request {
        model: "test-model".into(),
        messages: vec![Message::user("never persisted")],
        tools: vec![],
        max_tokens: None,
    };

    let recorded = store.record_request(&request, "faux", serde_json::Value::Null, RequestSource::Projection);

    assert!(recorded.is_err());
    assert_eq!(store.entries().len(), before);
}
