//! `Hooks::prepare_request` (pi: `prepareRequest`): hooks change the model or reasoning level
//! of one task request, never its context, and the audit records what each request used.

mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use common::{EchoTool, RunnerSetup, config, fast_retry, header, replayed_requests};
use zx_agent::agent::{AgentEvent, HookContext, HookResult, Hooks, RequestPlan, RequestSettings, Tool};
use zx_agent::ai::Reasoning;
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::runner::RunOutcome;
use zx_agent::session::SessionStore;

/// Uses `model` with high reasoning on the first turn only, and records what it saw.
struct FirstTurnUpgrade {
    model: &'static str,
    seen: Arc<Mutex<Vec<(u32, RequestSettings, usize)>>>,
}

#[async_trait]
impl Hooks for FirstTurnUpgrade {
    async fn prepare_request(&self, plan: &RequestPlan<'_>, _ctx: &HookContext) -> HookResult<Option<RequestSettings>> {
        self.seen
            .lock()
            .unwrap()
            .push((plan.turn, plan.settings.clone(), plan.messages.len()));
        Ok((plan.turn == 1).then(|| RequestSettings {
            model: self.model.into(),
            reasoning: Some(Reasoning::High),
        }))
    }
}

/// Lowers the reasoning level that an earlier hook chose, keeping its model.
struct Lower;

#[async_trait]
impl Hooks for Lower {
    async fn prepare_request(&self, plan: &RequestPlan<'_>, _ctx: &HookContext) -> HookResult<Option<RequestSettings>> {
        Ok(plan.settings.reasoning.map(|_| RequestSettings {
            reasoning: Some(Reasoning::Low),
            ..plan.settings.clone()
        }))
    }
}

#[tokio::test]
async fn a_hook_changes_the_model_and_reasoning_of_one_request_and_the_audit_records_them() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c1", "echo", json!({"text": "hi"})),
        FauxStep::text("done"),
    ]));
    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(EchoTool::new("echo"))];
    let mut loop_config = config(provider.clone(), tools);
    let seen = Arc::new(Mutex::new(Vec::new()));
    loop_config.hooks.push(FirstTurnUpgrade {
        model: "big-model",
        seen: seen.clone(),
    });
    let (mut runner, events) = RunnerSetup {
        session: SessionStore::create(&path, header()).unwrap(),
        ..RunnerSetup::new(loop_config)
    }
    .build();

    let report = runner.run("go", &CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    let sent: Vec<(String, Option<Reasoning>)> = provider
        .requests()
        .iter()
        .map(|request| (request.model.clone(), request.reasoning))
        .collect();
    // The change lasts one request: the second turn is back to the configured settings.
    assert_eq!(
        sent,
        [
            ("big-model".to_string(), Some(Reasoning::High)),
            ("test-model".to_string(), None)
        ]
    );
    // The hook saw the configured settings and the context about to be sent.
    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].0, 1);
    assert_eq!(seen[0].1.model, "test-model");
    assert_eq!(seen[0].2, provider.requests()[0].messages.len());
    let started: Vec<String> = events
        .events()
        .iter()
        .filter_map(|event| match event {
            AgentEvent::RequestStart { model, .. } => Some(model.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(started, ["big-model", "test-model"]);
    // Every request rebuilds from the session file with the settings it used.
    let replayed = replayed_requests(&SessionStore::read(&path).unwrap());
    assert_eq!(replayed, provider.requests());
}

#[tokio::test]
async fn hooks_chain_their_changes_in_order() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("done")]));
    let mut loop_config = config(provider.clone(), vec![]);
    loop_config.hooks.push(FirstTurnUpgrade {
        model: "big-model",
        seen: Arc::default(),
    });
    loop_config.hooks.push(Lower);
    let (mut runner, _) = RunnerSetup::new(loop_config).build();

    runner.run("go", &CancellationToken::new()).await;

    let request = &provider.requests()[0];
    assert_eq!(
        (request.model.as_str(), request.reasoning),
        ("big-model", Some(Reasoning::Low))
    );
}

/// Fails, or chooses an empty model.
enum Broken {
    Fails,
    EmptyModel,
}

#[async_trait]
impl Hooks for Broken {
    async fn prepare_request(
        &self,
        _plan: &RequestPlan<'_>,
        _ctx: &HookContext,
    ) -> HookResult<Option<RequestSettings>> {
        match self {
            Broken::Fails => Err("no model available".into()),
            Broken::EmptyModel => Ok(Some(RequestSettings {
                model: " ".into(),
                reasoning: None,
            })),
        }
    }
}

#[tokio::test]
async fn a_failing_hook_or_an_empty_model_ends_the_run_before_the_request_without_retrying() {
    for (hook, expected) in [
        (Broken::Fails, "prepare_request failed: no model available"),
        (Broken::EmptyModel, "prepare_request failed: chose an empty model"),
    ] {
        let provider = Arc::new(FauxProvider::new([FauxStep::text("must not be requested")]));
        let mut loop_config = config(provider.clone(), vec![]);
        loop_config.hooks.push(hook);
        let (mut runner, _) = RunnerSetup {
            retry: fast_retry(3),
            ..RunnerSetup::new(loop_config)
        }
        .build();

        let report = runner.run("go", &CancellationToken::new()).await;

        assert_eq!(report.outcome, RunOutcome::Error);
        assert_eq!(report.retries, 0);
        let error = report.error.unwrap_or_default();
        assert!(error.contains(expected), "{error}");
        assert!(provider.requests().is_empty());
    }
}

struct Panics;

#[async_trait]
impl Hooks for Panics {
    async fn prepare_request(
        &self,
        _plan: &RequestPlan<'_>,
        _ctx: &HookContext,
    ) -> HookResult<Option<RequestSettings>> {
        panic!("hook bug");
    }
}

#[tokio::test]
async fn a_panicking_hook_stops_the_attempt_without_crashing_the_run() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("must not be requested")]));
    let mut loop_config = config(provider.clone(), vec![]);
    loop_config.hooks.push(Panics);
    let (mut runner, _) = RunnerSetup::new(loop_config).build();

    let report = runner.run("go", &CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Error);
    assert!(report.error.unwrap_or_default().contains("prepare_request panicked"));
    assert!(provider.requests().is_empty());
}
