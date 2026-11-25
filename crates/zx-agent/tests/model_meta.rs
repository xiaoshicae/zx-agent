//! Model metadata and metering: prices give responses a cost, a context window
//! adds a token trigger to automatic summaries, and the cache key is audited.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::{
    AssistantMessage, Message, ModelCost, ModelMeta, Provider, ProviderErrorKind, Request, StopReason, StreamEvent,
    Usage,
};
use zx_agent::ai::{EventReceiver, event_channel, send_event};
use zx_agent::compaction::{self, Config, TokenTrigger};
use zx_agent::events::RunOutcome;
use zx_agent::hooks::{ContextEdit, Hook, HookContext, HookResult};
use zx_agent::transcript::{Entry, SessionHeader, Transcript};
use zx_testing::{
    EchoTool, RunnerSetup, Seed, answer_reporting, config, header, read_lines, seeded_session, summary_json,
};

/// 1 per input token and 2 per output token, in any currency: the faux provider reports 10 in, 5 out.
fn priced() -> ModelMeta {
    ModelMeta {
        context_window: None,
        cost: Some(ModelCost {
            input: 1_000_000.0,
            output: 2_000_000.0,
            cache_read: 0.0,
            cache_write: 0.0,
        }),
        takes_images: false,
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
    let (runner, _) = RunnerSetup {
        session: zx_testing::create_session_file(&file, header()).await.unwrap().into(),
        ..RunnerSetup::new(config)
    }
    .build()
    .await;

    let report = runner.run("go").await;

    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(report.usage.total_cost, Some(40.0), "two answers at 20 each");
    let costs: Vec<Option<f64>> = runner
        .transcript()
        .messages()
        .iter()
        .filter_map(|message| match message {
            Message::Assistant(answer) => Some(answer.usage.total_cost),
            _ => None,
        })
        .collect();
    assert_eq!(costs, [Some(20.0), Some(20.0)]);
    drop(runner);
    let recorded = read_lines(&file)
        .into_iter()
        .filter(|line| line["message"]["role"] == "assistant")
        .map(|line| line["message"]["usage"]["totalCost"].as_f64())
        .collect::<Vec<_>>();
    assert_eq!(recorded, [Some(20.0), Some(20.0)]);
}

#[tokio::test]
async fn a_model_without_prices_has_no_cost() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("done")]));
    let mut config = config(provider, vec![]);
    config.models.insert("another-model".into(), priced());
    let (runner, _) = RunnerSetup::new(config).build().await;
    let report = runner.run("go").await;
    assert_eq!(report.usage.total_cost, None);
    assert!(
        serde_json::to_value(&report.usage).unwrap().get("totalCost").is_none(),
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
    let (seed, old) = seeded_session().await;
    let provider = Arc::new(RenamingProvider {
        faux: FauxProvider::new([FauxStep::text(summary_json(&old)), FauxStep::text("done")]),
        served: "test-model-2026-01-01",
    });
    let mut config = config(provider, vec![]);
    config.models.insert("test-model".into(), priced());
    let (runner, _) = RunnerSetup {
        session: seed.into(),
        compaction: Config {
            keep_recent_bytes: 1,
            ..Config::default()
        },
        ..RunnerSetup::new(config)
    }
    .build()
    .await;

    let compacted = runner.compact().await.unwrap();
    let report = runner.run("go").await;

    assert_eq!(compacted.outcome, compaction::Outcome::Completed, "{compacted:?}");
    assert_eq!(
        compacted.usage.total_cost,
        Some(20.0),
        "the summary is priced by the requested model"
    );
    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    assert_eq!(report.usage.total_cost, Some(20.0), "so is the answer");
    let recorded = runner.transcript().latest_compaction().unwrap().usage.total_cost;
    assert_eq!(recorded, Some(20.0), "the compaction record carries the cost");
}

/// Compaction by a token trigger of `tokens` alone, keeping 1 byte of recent messages.
fn only_token_trigger(tokens: u64) -> Config {
    Config {
        trigger_bytes: None,
        trigger_tokens: Some(TokenTrigger::Above(tokens)),
        keep_recent_bytes: 1,
        timeout: Duration::from_secs(1),
        ..Config::default()
    }
}

async fn run_with_token_trigger(trigger_tokens: u64) -> (RunOutcome, Vec<&'static str>, usize) {
    let (seed, old) = seeded_session().await;
    let provider = Arc::new(FauxProvider::new([
        FauxStep::text(summary_json(&old)),
        FauxStep::text("answer"),
    ]));
    let (runner, events) = RunnerSetup {
        session: seed.into(),
        compaction: only_token_trigger(trigger_tokens),
        ..RunnerSetup::new(config(provider.clone(), vec![]))
    }
    .build()
    .await;
    let report = runner.run("continue").await;
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

#[tokio::test]
async fn no_summary_is_paid_for_when_the_latest_input_alone_passes_the_token_trigger() {
    let mut seed = Seed::in_memory(header()).await;
    // Something old enough to summarize, though no summary would be enough.
    seed.append_old_observation().await;
    let provider = Arc::new(FauxProvider::new([FauxStep::text("answer")]));
    let (runner, _) = RunnerSetup {
        session: seed.into(),
        compaction: only_token_trigger(100),
        ..RunnerSetup::new(config(provider.clone(), vec![]))
    }
    .build()
    .await;

    // Some 1.4 KiB, which estimate to about 350 tokens: a summary keeps the latest input
    // whole, so none can bring the context under the trigger. The context goes to the model
    // as it is, which alone can say whether it is too long.
    let report = runner.run("latest ".repeat(200)).await;

    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    assert_eq!(report.final_text, "answer");
    assert_eq!(provider.requests().len(), 1, "the task request, and no summary");
}

#[tokio::test]
async fn a_summary_that_leaves_the_context_above_the_token_trigger_is_not_kept() {
    let (seed, old) = seeded_session().await;
    // About 600 bytes of summary, which alone estimate to some 150 tokens.
    let long_summary =
        json!({"facts":[{"text":"observed ".repeat(66),"sourceEntryIds":[old]}],"inferences":[],"plans":[]});
    let provider = Arc::new(FauxProvider::new([
        FauxStep::text(long_summary.to_string()),
        FauxStep::text("answer"),
    ]));
    let (runner, _) = RunnerSetup {
        session: seed.into(),
        compaction: only_token_trigger(100),
        ..RunnerSetup::new(config(provider.clone(), vec![]))
    }
    .build()
    .await;

    let report = runner.run("continue").await;

    // The summary would not help, so the context stays as it was and the run goes on.
    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    assert_eq!(report.final_text, "answer");
    assert_eq!(
        provider.requests().len(),
        2,
        "the summary request, then the task request"
    );
    assert!(runner.transcript().latest_compaction().is_none());
    let failed = runner.transcript().entries().iter().find_map(|entry| match entry {
        Entry::CompactionFailed(failed) => Some(failed.error.clone()),
        _ => None,
    });
    assert!(
        failed
            .as_deref()
            .unwrap_or_default()
            .contains("still exceeds trigger_tokens"),
        "{failed:?}"
    );
}

/// A token trigger of 1000 that keeps about 1 KiB of recent messages.
fn token_trigger() -> Config {
    Config {
        trigger_tokens: Some(TokenTrigger::Above(1_000)),
        keep_recent_bytes: 1_024,
        timeout: Duration::from_secs(1),
        ..Config::default()
    }
}

#[tokio::test]
async fn a_summary_is_not_repeated_for_the_usage_an_answer_reported_before_it() {
    // An old message worth summarizing, then an answer that reported a context above the
    // trigger and is recent enough for the summary to keep.
    let mut seed = Seed::in_memory(header()).await;
    let old = seed.append_old_observation().await;
    seed.append_message(answer_reporting(5_000)).await.unwrap();
    let provider = Arc::new(FauxProvider::new([
        FauxStep::text(summary_json(&old)),
        FauxStep::text("answer"),
    ]));
    let (runner, events) = RunnerSetup {
        session: seed.into(),
        compaction: token_trigger(),
        ..RunnerSetup::new(config(provider.clone(), vec![]))
    }
    .build()
    .await;

    let report = runner.run("continue").await;

    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    let compactions = events.kinds().iter().filter(|kind| **kind == "compaction_end").count();
    assert_eq!(compactions, 1, "{:?}", events.kinds());
    assert_eq!(
        provider.requests().len(),
        2,
        "the summary request, then the task request"
    );
}

/// Replaces the first long input with a short text, as a hook that trims old output does.
struct TrimLongInput;

#[async_trait]
impl Hook for TrimLongInput {
    async fn transform_context(&self, messages: &[Message], _ctx: &HookContext) -> HookResult<Vec<ContextEdit>> {
        let long = messages
            .iter()
            .position(|message| matches!(message, Message::User { content, .. } if content.len() > 100));
        Ok(long
            .map(|index| ContextEdit::ReplaceText {
                index,
                text: "trimmed".into(),
            })
            .into_iter()
            .collect())
    }
}

#[tokio::test]
async fn the_token_trigger_follows_an_edit_made_after_the_last_reported_usage() {
    // The last answer reported a context above the trigger. A hook then trims the message
    // that made it large, and what is left is too recent to compact.
    let mut seed = Seed::in_memory(header()).await;
    seed.append_old_observation().await;
    seed.append_message(answer_reporting(5_000)).await.unwrap();
    let provider = Arc::new(FauxProvider::new([FauxStep::text("answer")]));
    let mut config = config(provider.clone(), vec![]);
    config.hooks.push("trim_long_input", Arc::new(TrimLongInput));
    let (runner, events) = RunnerSetup {
        session: seed.into(),
        compaction: token_trigger(),
        ..RunnerSetup::new(config)
    }
    .build()
    .await;

    let report = runner.run("continue").await;

    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    assert!(!events.kinds().contains(&"compaction_start"), "{:?}", events.kinds());
    assert_eq!(provider.requests().len(), 1);
}

/// An in-memory session that holds `messages`.
async fn session_of(messages: &[Message]) -> Transcript {
    let mut transcript = Transcript::in_memory(header());
    for message in messages {
        transcript.append_message(message.clone()).await.unwrap();
    }
    transcript
}

#[tokio::test]
async fn the_context_estimate_starts_from_the_last_reported_usage() {
    let counted = AssistantMessage {
        usage: Usage {
            input: 100,
            output: 20,
            total_tokens: 120,
            ..Usage::default()
        },
        ..AssistantMessage::new("p", "m", StopReason::Stop, Vec::new())
    };
    let messages = vec![
        Message::user("a".repeat(400)),
        Message::Assistant(counted.clone()),
        Message::user("b".repeat(80)),
    ];
    let after = compaction::context_bytes(&messages[2..]).unwrap();
    assert_eq!(
        compaction::context_tokens(&session_of(&messages).await).unwrap(),
        120 + after.div_ceil(4) as u64
    );
    // A failed answer reports nothing usable: every byte is estimated.
    let mut failed = counted;
    failed.stop_reason = StopReason::Error;
    let messages = vec![Message::user("a".repeat(400)), Message::Assistant(failed)];
    let all = compaction::context_bytes(&messages).unwrap();
    assert_eq!(
        compaction::context_tokens(&session_of(&messages).await).unwrap(),
        all.div_ceil(4) as u64
    );
    assert_eq!(compaction::context_tokens(&session_of(&[]).await).unwrap(), 1);
}

/// What is known about a model whose context holds `tokens`, without prices.
fn window(tokens: u64) -> ModelMeta {
    ModelMeta {
        context_window: Some(tokens),
        cost: None,
        takes_images: false,
    }
}

/// Run "continue" in a session with some 1400 tokens of context, whose token trigger lies 1000
/// tokens below the window of the model it asks, after switching from the agent's model to
/// `model` when one is given. Returns the kinds of the run's events and the requests made.
async fn run_after_switching_to(model: Option<&str>) -> (Vec<&'static str>, usize) {
    let (seed, old) = seeded_session().await;
    let provider = Arc::new(FauxProvider::new([
        FauxStep::text(summary_json(&old)),
        FauxStep::text("answer"),
    ]));
    let mut config = config(provider.clone(), vec![]);
    // The context lies far below what the window of the agent's model leaves under the
    // reserve, and above what the window of small-model leaves.
    config.models.insert("test-model".into(), window(1_000_000));
    config.models.insert("small-model".into(), window(2_000));
    let (runner, events) = RunnerSetup {
        session: seed.into(),
        compaction: Config {
            trigger_tokens: Some(TokenTrigger::BelowWindow { reserve_tokens: 1_000 }),
            keep_recent_bytes: 1,
            timeout: Duration::from_secs(1),
            ..Config::default()
        },
        ..RunnerSetup::new(config)
    }
    .build()
    .await;
    if let Some(model) = model {
        runner.set_model(provider.clone(), model).unwrap();
    }

    let report = runner.run("continue").await;

    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    (events.kinds(), provider.requests().len())
}

#[tokio::test]
async fn the_token_trigger_follows_the_window_of_the_model_the_session_switches_to() {
    let (kinds, requests) = run_after_switching_to(None).await;
    assert!(!kinds.contains(&"compaction_start"), "{kinds:?}");
    assert_eq!(requests, 1);

    let (kinds, requests) = run_after_switching_to(Some("small-model")).await;
    assert!(kinds.contains(&"compaction_start"), "{kinds:?}");
    assert_eq!(requests, 2, "the summary request, then the task request");
}

#[tokio::test]
async fn the_cache_key_goes_to_the_provider_and_into_the_request_audit() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("session.jsonl");
    let provider = Arc::new(FauxProvider::new([FauxStep::text("done")]));
    let config = config(provider.clone(), vec![]);
    let header = SessionHeader {
        id: "session-7".into(),
        ..header()
    };
    let (runner, _) = RunnerSetup {
        session: zx_testing::create_session_file(&file, header).await.unwrap().into(),
        ..RunnerSetup::new(config)
    }
    .build()
    .await;
    runner.run("go").await;
    assert_eq!(provider.requests()[0].cache_key.as_deref(), Some("session-7"));
    let replayed = runner
        .transcript()
        .entries()
        .iter()
        .find_map(|entry| match entry {
            Entry::RequestStarted(started) => Some(runner.transcript().replay_request(&started.id).unwrap().1),
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

#[tokio::test]
async fn a_cost_budget_refuses_an_unpriced_model_after_switching() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("must not request")]));
    let mut config = config(provider.clone(), vec![]);
    config.models.insert("test-model".into(), priced());
    let (runner, _) = RunnerSetup {
        limits: zx_agent::agent::RunLimits {
            cost_budget: Some(1.0),
            ..zx_testing::test_limits()
        },
        ..RunnerSetup::new(config)
    }
    .build()
    .await;
    runner.set_model(provider.clone(), "unpriced").unwrap();
    let report = runner.run("go").await;
    assert_eq!(report.outcome, RunOutcome::Failed);
    assert!(report.error.as_deref().unwrap().contains("unpriced"));
    assert!(
        provider.requests().is_empty(),
        "no unmetered request may leave the process"
    );
}

struct SelectModel(&'static str);

#[async_trait]
impl Hook for SelectModel {
    async fn prepare_request(
        &self,
        plan: &zx_agent::hooks::RequestPlan<'_>,
        _ctx: &HookContext,
    ) -> HookResult<Option<zx_agent::hooks::RequestSettings>> {
        Ok(Some(zx_agent::hooks::RequestSettings {
            model: self.0.into(),
            ..plan.settings.clone()
        }))
    }
}

#[tokio::test]
async fn a_cost_budget_checks_the_model_selected_by_a_request_hook() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("must not request")]));
    let mut config = config(provider.clone(), vec![]);
    config.models.insert("test-model".into(), priced());
    config.hooks.push("select_unpriced", Arc::new(SelectModel("unpriced")));
    let (runner, _) = RunnerSetup {
        limits: zx_agent::agent::RunLimits {
            cost_budget: Some(1.0),
            ..zx_testing::test_limits()
        },
        ..RunnerSetup::new(config)
    }
    .build()
    .await;
    let report = runner.run("go").await;
    assert_eq!(report.outcome, RunOutcome::Failed);
    assert!(report.error.unwrap().contains("unpriced"));
    assert!(provider.requests().is_empty());
}

#[tokio::test]
async fn a_cost_budget_refuses_an_unpriced_automatic_summary() {
    let (session, _) = seeded_session().await;
    let provider = Arc::new(FauxProvider::new([FauxStep::text("must not request")]));
    let (runner, _) = RunnerSetup {
        session: session.into(),
        limits: zx_agent::agent::RunLimits {
            cost_budget: Some(1.0),
            ..zx_testing::test_limits()
        },
        compaction: Config {
            trigger_bytes: Some(1024),
            keep_recent_bytes: 1,
            ..Config::default()
        },
        ..RunnerSetup::new(config(provider.clone(), vec![]))
    }
    .build()
    .await;
    let report = runner.run("go").await;
    assert_eq!(report.outcome, RunOutcome::Failed);
    assert!(report.error.unwrap().contains("prices"));
    assert!(provider.requests().is_empty());
}

#[tokio::test]
async fn overflow_recovery_checks_the_summary_models_prices() {
    let (session, _) = seeded_session().await;
    let provider = Arc::new(FauxProvider::new([
        FauxStep::fail(ProviderErrorKind::ContextOverflow, "maximum context length exceeded"),
        FauxStep::text("must not request"),
    ]));
    let mut config = config(provider.clone(), vec![]);
    config.models.insert("priced".into(), priced());
    config.hooks.push("priced_task", Arc::new(SelectModel("priced")));
    let (runner, _) = RunnerSetup {
        session: session.into(),
        limits: zx_agent::agent::RunLimits {
            cost_budget: Some(1.0),
            ..zx_testing::test_limits()
        },
        compaction: Config {
            trigger_bytes: Some(1_000_000),
            keep_recent_bytes: 1,
            ..Config::default()
        },
        ..RunnerSetup::new(config)
    }
    .build()
    .await;
    let report = runner.run("go").await;
    assert_eq!(
        provider.requests().len(),
        1,
        "only the priced task request may leave the process"
    );
    assert_eq!(report.outcome, RunOutcome::Failed);
    assert!(report.error.unwrap().contains("prices"));
}

#[tokio::test]
async fn a_cost_budget_refuses_an_unpriced_manual_summary_after_switching() {
    let (session, _) = seeded_session().await;
    let provider = Arc::new(FauxProvider::new([FauxStep::text("must not request")]));
    let mut config = config(provider.clone(), vec![]);
    config.models.insert("test-model".into(), priced());
    let (runner, _) = RunnerSetup {
        session: session.into(),
        limits: zx_agent::agent::RunLimits {
            cost_budget: Some(1.0),
            ..zx_testing::test_limits()
        },
        compaction: Config {
            keep_recent_bytes: 1,
            ..Config::default()
        },
        ..RunnerSetup::new(config)
    }
    .build()
    .await;
    runner.set_model(provider.clone(), "unpriced").unwrap();
    let report = runner.compact().await.unwrap();
    assert!(
        provider.requests().is_empty(),
        "manual summaries must not dispatch an unpriced model"
    );
    assert_eq!(report.outcome, compaction::Outcome::Failed);
    assert!(report.error.unwrap().contains("unpriced"));
}
