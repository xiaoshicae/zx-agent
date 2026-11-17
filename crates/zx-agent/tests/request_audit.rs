//! Request audit (G3): every model request can be rebuilt from the session file.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::{Message, PREAMBLE, Request, SystemPrompt};
use zx_agent::compaction::{Config, Outcome};
use zx_agent::events::RunOutcome;
use zx_agent::tools::Tool;
use zx_agent::transcript::{Entry, LoadedSession, RequestSource, Transcript};

use common::{EchoTool, RunnerSetup, config, header, read_lines, replayed_requests};

/// Every recorded request of `store`, rebuilt: the provider-neutral request and its
/// OpenAI-compatible body in both system update modes.
fn replays(store: &Transcript) -> serde_json::Value {
    let mut out = Vec::new();
    for id in request_ids(store) {
        let (config, request) = store.replay_request(&id).unwrap();
        let mut bodies = serde_json::Map::new();
        for mode in ["collapse", "inline"] {
            let mut options = config.provider_options.clone();
            options["systemUpdates"] = json!(mode);
            let body = zx_agent::ai::openai::replay_body(&request, &config.provider, &options).unwrap();
            bodies.insert(mode.into(), body);
        }
        out.push(json!({"entryId": id, "request": request, "body": bodies}));
    }
    serde_json::Value::Array(out)
}

/// A recorded session must keep rebuilding to the requests it sent. The fixture holds a tool
/// call, a retried failure hidden by a context edit, a full summary and a request after it.
/// If a change to the projection, to summaries or to the OpenAI conversion alters this
/// output, sessions written before it would no longer show what they sent. Only the current
/// format is read, so raise `SESSION_VERSION`, or the request conversion version for a
/// change to the OpenAI conversion, so that such sessions are refused instead; then update
/// the fixture's header or options and regenerate the expected file.
#[test]
fn a_recorded_session_rebuilds_to_the_requests_it_sent() {
    assert_fixture_rebuilds("replay-full");
}

/// `crates/zx-agent/tests/fixtures/<name>.jsonl` rebuilds to `<name>.expected.json` beside it,
/// byte for byte as pretty-printed JSON, so that the order of fields counts too.
fn assert_fixture_rebuilds(name: &str) {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let store = common::read_session_file(&fixtures.join(format!("{name}.jsonl"))).unwrap();
    let expected = std::fs::read_to_string(fixtures.join(format!("{name}.expected.json"))).unwrap();

    let rebuilt = format!("{}\n", serde_json::to_string_pretty(&replays(&store)).unwrap());

    assert!(rebuilt == expected, "rebuilt requests changed:\n{rebuilt}");
}

/// Request checkpoint IDs on the session's entry list, in file order.
fn request_ids(store: &Transcript) -> Vec<String> {
    store
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            Entry::RequestStarted(started) => Some(started.id.clone()),
            _ => None,
        })
        .collect()
}

/// Every request rebuilt from a fresh snapshot of the file.
fn replayed(path: &Path) -> Vec<Request> {
    replayed_requests(&common::read_session_file(path).unwrap())
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
    config.system_prompt = SystemPrompt::default().with_section(PREAMBLE, "Be brief.");
    config.max_tokens = Some(256);
    let (runner, _) = RunnerSetup {
        session: common::create_session_file(&path, header()).await.unwrap(),
        ..RunnerSetup::new(config)
    }
    .build();

    let report = runner.run("inspect").await;

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
    let (session, _) = RunnerSetup {
        session: common::create_session_file(&path, header()).await.unwrap(),
        ..RunnerSetup::new(config(first.clone(), vec![]))
    }
    .build();

    session.prompt("a").await.unwrap();
    session.prompt("b").await.unwrap();
    session.set_model(second.clone(), "other-model").unwrap();
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
async fn summary_requests_are_rebuilt_from_their_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let mut store = common::create_session_file(&path, header()).await.unwrap();
    let old = store
        .append_message(Message::user("observed ".repeat(600)))
        .await
        .unwrap();
    store.append_message(Message::user("most recent input")).await.unwrap();
    let summary =
        json!({"facts":[{"text":"Old observation","sourceEntryIds":[old]}],"inferences":[],"plans":[]}).to_string();
    let provider = Arc::new(FauxProvider::new([FauxStep::text(summary)]));
    let (session, _) = RunnerSetup {
        session: store,
        compaction: Config {
            keep_recent_bytes: 1,
            timeout: Duration::from_secs(1),
            ..Config::default()
        },
        ..RunnerSetup::new(config(provider.clone(), vec![]))
    }
    .build();

    let report = session.compact().await.unwrap();

    assert_eq!(report.outcome, Outcome::Completed, "{report:?}");
    let reopened = common::read_session_file(&path).unwrap();
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

#[tokio::test]
async fn only_request_checkpoints_can_be_replayed() {
    let mut store = Transcript::in_memory(header());
    let message = store.append_message(Message::user("not a request")).await.unwrap();
    assert!(store.replay_request(&message).is_err());
    assert!(store.replay_request("missing").is_err());
}

#[tokio::test]
async fn requests_around_a_recovery_from_context_overflow_are_rebuilt_from_the_session_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let mut session = common::create_session_file(&path, header()).await.unwrap();
    let old = session
        .append_message(Message::user("observed ".repeat(600)))
        .await
        .unwrap();
    let summary =
        json!({"facts":[{"text":"Old observation","sourceEntryIds":[old]}],"inferences":[],"plans":["Continue"]});
    let provider = Arc::new(FauxProvider::new([
        FauxStep::Fail("HTTP 400: This model's maximum context length is 65536 tokens".into()),
        FauxStep::text(summary.to_string()),
        FauxStep::text("answer"),
    ]));
    let compaction = Config {
        trigger_bytes: Some(100_000),
        keep_recent_bytes: 1,
        ..Config::default()
    };
    let (runner, _) = RunnerSetup {
        session,
        compaction,
        ..RunnerSetup::new(config(provider.clone(), vec![]))
    }
    .build();

    let report = runner.run("continue").await;

    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    // The failed task request, the summary and the task request after it.
    let sent = provider.requests();
    assert_eq!(sent.len(), 3);
    assert_eq!(replayed(&path), sent);
}

/// A request for `model` and nothing else.
fn request_for(model: &str) -> Request {
    Request {
        model: model.into(),
        messages: vec![],
        tools: vec![],
        max_tokens: None,
        reasoning: None,
        cache_key: None,
    }
}

/// The IDs of the request configurations `store` recorded, oldest first.
fn config_ids(store: &Transcript) -> Vec<String> {
    store
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            Entry::RequestConfig(entry) => Some(entry.id.clone()),
            _ => None,
        })
        .collect()
}

/// Why the records of `store` do not load once a request checkpoint after `parent` names the
/// configuration `config`.
fn refusal_of_a_request(store: &Transcript, parent: &str, config: &str) -> String {
    let mut records: Vec<String> = store
        .entries()
        .iter()
        .map(|entry| serde_json::to_string(entry).unwrap())
        .collect();
    records.push(
        json!({"type":"request_started", "id":"invalid", "parentId":parent,
        "timestamp":0, "configId":config, "source":{"kind":"projection"}})
        .to_string(),
    );
    let loaded = LoadedSession {
        header: store.header().clone(),
        records,
        incomplete_tail_bytes: 0,
    };
    let error = Transcript::snapshot(loaded).err().expect("the request must be refused");
    format!("{error:#}")
}

#[tokio::test]
async fn request_configuration_indexes_keep_branches_separate_when_replaying() {
    let mut store = Transcript::in_memory(header());
    let first = store
        .record_request(&request_for("first"), "p", json!({}), RequestSource::Projection)
        .await
        .unwrap();
    store
        .record_request(&request_for("other"), "p", json!({}), RequestSource::Projection)
        .await
        .unwrap();
    let foreign = config_ids(&store).pop().unwrap();
    store.branch(&first).unwrap();
    let last = store
        .record_request(&request_for("first"), "p", json!({}), RequestSource::Projection)
        .await
        .unwrap();
    assert_eq!(config_ids(&store).len(), 2, "the branch reuses its configuration");

    let error = refusal_of_a_request(&store, &last, &foreign);

    assert!(error.contains("is not the one in force on this branch"), "{error}");
}

#[tokio::test]
async fn a_request_names_the_configuration_in_force_not_an_earlier_one_of_its_branch() {
    let mut store = Transcript::in_memory(header());
    store
        .record_request(&request_for("first"), "p", json!({}), RequestSource::Projection)
        .await
        .unwrap();
    let last = store
        .record_request(&request_for("other"), "p", json!({}), RequestSource::Projection)
        .await
        .unwrap();
    let configs = config_ids(&store);
    assert_eq!(configs.len(), 2);

    let error = refusal_of_a_request(&store, &last, &configs[0]);

    assert!(error.contains("is not the one in force on this branch"), "{error}");
}
