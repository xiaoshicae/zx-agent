//! End-to-end tests against a real model API. They cost money, so they are opt-in twice:
//!
//! 1. every test is `#[ignore]`d, so `cargo test` never runs them;
//! 2. each test refuses to start, before any request, unless `ZX_E2E_AUTHORIZED=1` is set.
//!
//! Run them only with the API owner's consent:
//!
//! ```text
//! ZX_E2E_AUTHORIZED=1 \
//! ZX_E2E_BASE_URL=https://api.example.com/v1 ZX_E2E_MODEL=model-id ZX_E2E_API_KEY_ENV=NAME_OF_KEY_VARIABLE \
//!     cargo test --test real_api -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! Optional: `ZX_E2E_SYSTEM_UPDATES=inline` for a server known to accept system messages
//! mid-conversation (default `collapse`). Every test caps output tokens and turns, and prints
//! its token usage; with a small model the whole file uses a few thousand tokens. The tests use
//! only synthetic prompts and files in temporary directories.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use common::{RunnerSetup, config, fast_retry};
use zx_agent::agent::CommandPolicy;
use zx_agent::ai::Message;
use zx_agent::ai::openai::{OpenAiCompatible, OpenAiConfig, SystemUpdates};
use zx_agent::prompt::{build_system_prompt, environment_section};
use zx_agent::runner::{Limits, RunOutcome, RunReport};
use zx_agent::session::SessionStore;
use zx_agent::tools::BashTool;

const PAID: &str = "calls a paid model API; needs ZX_E2E_AUTHORIZED=1 and the API owner's consent";

/// The API under test, from the environment.
struct Endpoint {
    base_url: String,
    model: String,
    api_key_env: String,
    system_updates: SystemUpdates,
}

impl Endpoint {
    /// Panics, before any request is made, unless the run is authorized and configured.
    fn from_env() -> Self {
        assert_eq!(
            std::env::var("ZX_E2E_AUTHORIZED").as_deref(),
            Ok("1"),
            "not authorized: this test {PAID}"
        );
        let required = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
        let system_updates = match std::env::var("ZX_E2E_SYSTEM_UPDATES").as_deref() {
            Ok("inline") => SystemUpdates::Inline,
            Ok("collapse") | Err(_) => SystemUpdates::Collapse,
            Ok(other) => panic!("ZX_E2E_SYSTEM_UPDATES must be inline or collapse, not {other:?}"),
        };
        Self {
            base_url: required("ZX_E2E_BASE_URL"),
            model: required("ZX_E2E_MODEL"),
            api_key_env: required("ZX_E2E_API_KEY_ENV"),
            system_updates,
        }
    }

    fn api_key(&self) -> String {
        std::env::var(&self.api_key_env).unwrap_or_else(|_| panic!("{} is not set", self.api_key_env))
    }

    /// A runner against this endpoint with small budgets, so a misbehaving model cannot run
    /// up costs: 256 output tokens per response, 4 turns, 2 minutes, no retries.
    fn setup(&self, api_key: String, cwd: &Path) -> RunnerSetup {
        let provider = OpenAiCompatible::new(OpenAiConfig {
            base_url: self.base_url.clone(),
            api_key: Some(api_key),
            system_updates: self.system_updates,
            ..OpenAiConfig::default()
        })
        .unwrap();
        let mut config = config(Arc::new(provider), vec![]);
        config.model = self.model.clone();
        config.cwd = cwd.to_path_buf();
        config.max_tokens = Some(256);
        config.system_prompt = build_system_prompt(
            "You are a test assistant. Follow the user's instructions exactly and keep answers short.",
            &[("environment", &environment_section(cwd))],
        );
        RunnerSetup {
            limits: Limits {
                max_turns: Some(4),
                timeout: Some(Duration::from_secs(120)),
                continue_on_length: 0,
            },
            ..RunnerSetup::new(config)
        }
    }

    /// `setup` with a bash tool limited to `allowed` programs. The key variable is hidden
    /// from commands.
    fn setup_with_bash(&self, cwd: &Path, allowed: &[&str]) -> RunnerSetup {
        let mut setup = self.setup(self.api_key(), cwd);
        let bash = BashTool {
            hidden_env: vec![self.api_key_env.clone()],
            ..BashTool::default()
        };
        setup.config.tools = vec![Arc::new(bash)];
        setup.config.policy = Arc::new(CommandPolicy::new(allowed));
        setup
    }
}

/// Print the cost of one test, visible with `--nocapture`.
fn print_usage(test: &str, report: &RunReport) {
    let usage = &report.usage;
    eprintln!(
        "[real_api] {test}: {} tokens (in {} / out {}), {} turns, outcome {:?}",
        usage.total_tokens, usage.input, usage.output, report.turns, report.outcome
    );
}

#[tokio::test]
#[ignore = "calls a paid model API; needs ZX_E2E_AUTHORIZED=1 and the API owner's consent"]
async fn answers_with_streamed_text_and_reports_usage() {
    let endpoint = Endpoint::from_env();
    let dir = tempfile::tempdir().unwrap();
    let (mut runner, _) = endpoint.setup(endpoint.api_key(), dir.path()).build();

    let report = runner
        .run("Reply with exactly the word PONG.", &CancellationToken::new())
        .await;
    print_usage("text", &report);

    assert_eq!(report.outcome, RunOutcome::Completed, "{:?}", report.error);
    assert!(
        report.final_text.to_uppercase().contains("PONG"),
        "{}",
        report.final_text
    );
    assert!(report.usage.output > 0 && report.usage.total_tokens > 0);
}

#[tokio::test]
#[ignore = "calls a paid model API; needs ZX_E2E_AUTHORIZED=1 and the API owner's consent"]
async fn runs_a_tool_and_answers_from_its_output() {
    let endpoint = Endpoint::from_env();
    let dir = tempfile::tempdir().unwrap();
    let (mut runner, _) = endpoint.setup_with_bash(dir.path(), &["seq", "wc"]).build();

    let prompt = "Use the bash tool to run `seq 1 7 | wc -l`, then reply with the number only.";
    let report = runner.run(prompt, &CancellationToken::new()).await;
    print_usage("tool round trip", &report);

    assert_eq!(report.outcome, RunOutcome::Completed, "{:?}", report.error);
    assert!(report.final_text.contains('7'), "{}", report.final_text);
    let ran = runner.session().messages().into_iter().any(
        |message| matches!(message, Message::ToolResult(result) if !result.is_error && result.content.contains('7')),
    );
    assert!(ran, "the bash tool ran successfully");
}

#[tokio::test]
#[ignore = "calls a paid model API; needs ZX_E2E_AUTHORIZED=1 and the API owner's consent"]
async fn a_policy_denial_reaches_the_model_and_the_run_completes() {
    let endpoint = Endpoint::from_env();
    let dir = tempfile::tempdir().unwrap();
    let (mut runner, _) = endpoint.setup_with_bash(dir.path(), &["seq"]).build();

    let prompt = "Call the bash tool once with the command `ls`, then say in one sentence whether it was blocked.";
    let report = runner.run(prompt, &CancellationToken::new()).await;
    print_usage("policy denial", &report);

    assert_eq!(report.outcome, RunOutcome::Completed, "{:?}", report.error);
    let denied = runner.session().messages().into_iter().any(|message| {
        matches!(message, Message::ToolResult(result) if result.is_error && result.content.starts_with("Blocked by policy"))
    });
    assert!(denied, "the model saw a policy denial");
}

#[tokio::test]
#[ignore = "calls a paid model API; needs ZX_E2E_AUTHORIZED=1 and the API owner's consent"]
async fn an_answer_over_the_output_limit_is_reported_as_truncated() {
    let endpoint = Endpoint::from_env();
    let dir = tempfile::tempdir().unwrap();
    let mut setup = endpoint.setup(endpoint.api_key(), dir.path());
    setup.config.max_tokens = Some(16);
    let (mut runner, _) = setup.build();

    let report = runner
        .run("Count from 1 to 200, separated by spaces.", &CancellationToken::new())
        .await;
    print_usage("truncation", &report);

    assert_eq!(report.outcome, RunOutcome::Truncated, "{:?}", report.error);
}

#[tokio::test]
#[ignore = "calls a paid model API; needs ZX_E2E_AUTHORIZED=1 and the API owner's consent"]
async fn a_resumed_session_gives_the_model_the_new_environment() {
    let endpoint = Endpoint::from_env();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");

    // First run in "first-dir"; the resumed run moves to "second-dir".
    let first_dir = dir.path().join("first-dir");
    std::fs::create_dir(&first_dir).unwrap();
    let (mut runner, _) = RunnerSetup {
        session: SessionStore::create(&path, common::header()).unwrap(),
        ..endpoint.setup(endpoint.api_key(), &first_dir)
    }
    .build();
    let first = runner.run("Reply with exactly: OK", &CancellationToken::new()).await;
    print_usage("resume, first run", &first);
    drop(runner);

    let second_dir = dir.path().join("second-dir");
    std::fs::create_dir(&second_dir).unwrap();
    let (mut runner, _) = RunnerSetup {
        session: SessionStore::open(&path).unwrap(),
        ..endpoint.setup(endpoint.api_key(), &second_dir)
    }
    .build();
    let prompt = "According to the system prompt, what is the working directory now? Reply with the path only.";
    let second = runner.run(prompt, &CancellationToken::new()).await;
    print_usage("resume, second run", &second);

    assert_eq!(second.outcome, RunOutcome::Completed, "{:?}", second.error);
    assert!(second.final_text.contains("second-dir"), "{}", second.final_text);
}

#[tokio::test]
#[ignore = "calls a paid model API; needs ZX_E2E_AUTHORIZED=1 and the API owner's consent"]
async fn an_invalid_key_fails_without_retrying() {
    let endpoint = Endpoint::from_env();
    let dir = tempfile::tempdir().unwrap();
    let (mut runner, _) = RunnerSetup {
        retry: fast_retry(2),
        ..endpoint.setup("zx-e2e-invalid-key".into(), dir.path())
    }
    .build();

    let report = runner.run("Reply with exactly: OK", &CancellationToken::new()).await;
    print_usage("invalid key", &report);

    assert_eq!((report.outcome, report.retries), (RunOutcome::Error, 0));
    let error = report.error.unwrap_or_default();
    assert!(error.starts_with("HTTP 4"), "{error}");
}
