//! Model metadata and metering (P10 item 8): prices give responses a cost, a context window
//! adds a token trigger to automatic summaries, and the cache key is audited.
mod common;

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use common::{EchoTool, RunnerSetup, config, header, read_lines, seeded_session, summary_json};
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::provider::{EventReceiver, event_channel, send_event};
use zx_agent::ai::{
    AssistantMessage, Message, ModelCost, ModelMeta, Provider, Request, StopReason, StreamEvent, Usage,
};
use zx_agent::compaction::{self, Config, DEFAULT_RESERVE_TOKENS};
use zx_agent::runner::RunOutcome;
use zx_agent::session::{Entry, SessionStore};

/// $1 per input token and $2 per output token: the faux provider reports 10 in, 5 out.
fn priced() -> ModelMeta {
    ModelMeta {
        context_window: None,
        cost: Some(ModelCost {
            input: 1_000_000.0,
            output: 2_000_000.0,
            cache_read: 0.0,
            cache_write: 0.0,
        }),
    }
}

#[tokio::test]
async fn responses_are_priced_by_the_configured_model_and_summed_in_the_report() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("session.jsonl");
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c1", "echo", json!({"text": "hi"})),
        FauxStep::text("done"),
    ]));
    let mut config = config(provider.clone(), vec![Arc::new(EchoTool::new("echo"))]);
    config.models.insert("test-model".into(), priced());
    let (mut runner, _) = RunnerSetup {
        session: SessionStore::create(&file, header()).unwrap(),
        ..RunnerSetup::new(config)
    }
    .build();

    let report = runner.run("go", &CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(report.usage.cost_usd, Some(40.0), "two answers at $20 each");
    let costs: Vec<Option<f64>> = runner
        .session()
        .messages()
        .iter()
        .filter_map(|message| match message {
            Message::Assistant(answer) => Some(answer.usage.cost_usd),
            _ => None,
        })
        .collect();
    assert_eq!(costs, [Some(20.0), Some(20.0)]);
    drop(runner);
    let recorded = read_lines(&file)
        .into_iter()
        .filter(|line| line["message"]["role"] == "assistant")
        .map(|line| line["message"]["usage"]["costUsd"].as_f64())
        .collect::<Vec<_>>();
    assert_eq!(recorded, [Some(20.0), Some(20.0)]);
}

#[tokio::test]
async fn a_model_without_prices_has_no_cost() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("done")]));
    let mut config = config(provider, vec![]);
    config.models.insert("another-model".into(), priced());
    let (mut runner, _) = RunnerSetup::new(config).build();
    let report = runner.run("go", &CancellationToken::new()).await;
    assert_eq!(report.usage.cost_usd, None);
    assert!(
        serde_json::to_value(&report.usage).unwrap().get("costUsd").is_none(),
        "an unknown cost is left out of the JSON"
    );
}

/// Answers like the faux provider, but names the model `served` in every response, as a
/// server that resolves an alias to a dated version does.
struct RenamingProvider {
    faux: FauxProvider,
    served: &'static str,
}

impl Provider for RenamingProvider {
    fn name(&self) -> &str {
        "renaming"
    }

    fn stream(&self, request: Request, cancel: CancellationToken) -> EventReceiver {
        let mut events = self.faux.stream(request, cancel);
        let served = self.served;
        let (mut tx, rx) = event_channel();
        tokio::spawn(async move {
            while let Some(event) = events.next().await {
                let event = match event {
                    StreamEvent::Done(mut message) => {
                        message.model = served.into();
                        StreamEvent::Done(message)
                    }
                    StreamEvent::Error(mut message) => {
                        message.model = served.into();
                        StreamEvent::Error(message)
                    }
                    passed @ (StreamEvent::Start | StreamEvent::Delta(_)) => passed,
                };
                if !send_event(&mut tx, event).await {
                    break;
                }
            }
        });
        rx
    }
}

#[tokio::test]
async fn costs_follow_the_requested_model_when_the_server_names_another() {
    let (store, old) = seeded_session().await;
    let provider = Arc::new(RenamingProvider {
        faux: FauxProvider::new([FauxStep::text(summary_json(&old)), FauxStep::text("done")]),
        served: "test-model-2026-01-01",
    });
    let mut config = config(provider, vec![]);
    config.models.insert("test-model".into(), priced());
    let (runner, _) = RunnerSetup {
        session: store,
        ..RunnerSetup::new(config)
    }
    .build();
    let mut runner = runner
        .with_compaction(Config {
            keep_recent_bytes: 1,
            ..Config::default()
        })
        .unwrap();

    let compacted = runner.compact(&CancellationToken::new()).await;
    let report = runner.run("go", &CancellationToken::new()).await;

    assert_eq!(compacted.outcome, compaction::Outcome::Completed, "{compacted:?}");
    assert_eq!(
        compacted.usage.cost_usd,
        Some(20.0),
        "the summary is priced by the requested model"
    );
    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    assert_eq!(report.usage.cost_usd, Some(20.0), "so is the answer");
    let recorded = runner.session().latest_compaction().unwrap().usage.cost_usd;
    assert_eq!(recorded, Some(20.0), "the compaction record carries the cost");
}

async fn run_with_token_trigger(trigger_tokens: u64) -> (RunOutcome, Vec<&'static str>, usize) {
    let (store, old) = seeded_session().await;
    let provider = Arc::new(FauxProvider::new([
        FauxStep::text(summary_json(&old)),
        FauxStep::text("answer"),
    ]));
    let (runner, events) = RunnerSetup {
        session: store,
        ..RunnerSetup::new(config(provider.clone(), vec![]))
    }
    .build();
    let mut runner = runner
        .with_compaction(Config {
            trigger_bytes: None,
            trigger_tokens: Some(trigger_tokens),
            keep_recent_bytes: 1,
            timeout: Duration::from_secs(1),
            ..Config::default()
        })
        .unwrap();
    let report = runner.run("continue", &CancellationToken::new()).await;
    (report.outcome, events.kinds(), provider.requests().len())
}

#[tokio::test]
async fn the_token_trigger_runs_a_summary_when_the_estimate_passes_it() {
    // Some 5 KiB of context estimate to over a thousand tokens.
    let (outcome, kinds, requests) = run_with_token_trigger(100).await;
    assert_eq!(outcome, RunOutcome::Completed);
    assert!(kinds.contains(&"compaction_start"), "{kinds:?}");
    assert_eq!(requests, 2, "the summary request, then the task request");

    let (outcome, kinds, requests) = run_with_token_trigger(100_000).await;
    assert_eq!(outcome, RunOutcome::Completed);
    assert!(!kinds.contains(&"compaction_start"), "{kinds:?}");
    assert_eq!(requests, 1);
}

#[test]
fn the_context_estimate_starts_from_the_last_reported_usage() {
    let mut counted = AssistantMessage::failure("p", "m", StopReason::Stop, "");
    counted.error_message = None;
    counted.usage = Usage {
        input: 100,
        output: 20,
        total_tokens: 120,
        ..Usage::default()
    };
    let messages = vec![
        Message::user("a".repeat(400)),
        Message::Assistant(counted.clone()),
        Message::user("b".repeat(80)),
    ];
    let after = compaction::context_bytes(&messages[2..]).unwrap();
    assert_eq!(
        compaction::context_tokens(&messages).unwrap(),
        120 + after.div_ceil(4) as u64
    );
    // A failed answer reports nothing usable: every byte is estimated.
    let mut failed = counted;
    failed.stop_reason = StopReason::Error;
    let messages = vec![Message::user("a".repeat(400)), Message::Assistant(failed)];
    let all = compaction::context_bytes(&messages).unwrap();
    assert_eq!(compaction::context_tokens(&messages).unwrap(), all.div_ceil(4) as u64);
    assert_eq!(compaction::context_tokens(&[]).unwrap(), 1);
}

#[test]
fn the_token_trigger_is_derived_from_a_context_window_below_its_reserve() {
    let mut config = Config::default();
    config.derive_trigger_tokens(100_000);
    assert_eq!(config.trigger_tokens, Some(100_000 - DEFAULT_RESERVE_TOKENS));
    config.derive_trigger_tokens(50_000);
    assert_eq!(
        config.trigger_tokens,
        Some(100_000 - DEFAULT_RESERVE_TOKENS),
        "an explicit value stays"
    );
    let mut small = Config {
        reserve_tokens: 1000,
        ..Config::default()
    };
    small.derive_trigger_tokens(1000);
    assert_eq!(small.trigger_tokens, None, "a window within the reserve sets nothing");
    // 20000 - 16384 tokens would lie within the 64 KiB of recent messages a summary keeps.
    let mut tight = Config::default();
    tight.derive_trigger_tokens(20_000);
    assert_eq!(
        tight.trigger_tokens, None,
        "no trigger a summary could never bring the context under"
    );
    assert!(
        Config {
            trigger_tokens: Some(100),
            ..Config::default()
        }
        .validate()
        .unwrap_err()
        .to_string()
        .contains("keepRecentBytes")
    );
    assert!(
        Config {
            trigger_tokens: Some(0),
            ..Config::default()
        }
        .validate()
        .is_err()
    );
    assert!(Config::default().validate().is_ok());
    assert!(!Config::default().automatic());
    assert!(small.automatic() || !small.automatic());
}

#[tokio::test]
async fn the_cache_key_goes_to_the_provider_and_into_the_request_audit() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("session.jsonl");
    let provider = Arc::new(FauxProvider::new([FauxStep::text("done")]));
    let mut config = config(provider.clone(), vec![]);
    config.cache_key = Some("session-7".into());
    let (mut runner, _) = RunnerSetup {
        session: SessionStore::create(&file, header()).unwrap(),
        ..RunnerSetup::new(config)
    }
    .build();
    runner.run("go", &CancellationToken::new()).await;
    assert_eq!(provider.requests()[0].cache_key.as_deref(), Some("session-7"));
    let replayed = runner
        .session()
        .entries()
        .iter()
        .find_map(|entry| match entry {
            Entry::RequestStarted(started) => Some(runner.session().replay_request(&started.id).unwrap().1),
            _ => None,
        })
        .unwrap();
    assert_eq!(replayed.cache_key.as_deref(), Some("session-7"));
    drop(runner);
    let config_line = read_lines(&file)
        .into_iter()
        .find(|line| line["type"] == "request_config")
        .unwrap();
    assert_eq!(config_line["config"]["cacheKey"], "session-7");
}
