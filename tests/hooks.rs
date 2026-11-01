//! Hook, approval and audit contracts, exercised through the public SDK.
mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use common::{EchoTool, RunnerSetup, config, fast_retry, header, read_lines, run_prompt, summarize};
use zx_agent::agent::{
    AgentEvent, ContextEdit, Decision, HookChain, HookResult, Hooks, LoopOutcome, ToolOutput, TurnDecision, TurnSummary,
};
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::{EventReceiver, Message, Provider, Request, SectionChange, StopReason, SystemMessage, ToolCall};
use zx_agent::extensions::{ContinueOnLength, RedactSecrets};
use zx_agent::interaction::{Approval, ApprovalRequest, Interaction};
use zx_agent::runner::{Limits, RunOutcome};
use zx_agent::session::{Entry, SessionStore};
use zx_agent::tools::BashTool;

struct Verdict(Decision);
#[async_trait]
impl Hooks for Verdict {
    async fn before_tool(&self, _call: &ToolCall) -> HookResult<Decision> {
        Ok(self.0.clone())
    }
}

#[tokio::test]
async fn a_later_allow_does_not_override_an_ask_without_a_human() {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c", "echo", json!({"text": "no"})),
        FauxStep::text("handled"),
    ]));
    let tool = Arc::new(EchoTool::new("echo"));
    let mut config = config(provider.clone(), vec![tool.clone()]);
    config.hooks.push(Verdict(Decision::Ask("review required".into())));
    config.hooks.push(Verdict(Decision::Allow));
    let (result, _) = run_prompt(&config, &mut Vec::new(), "go").await;
    assert_eq!(result.outcome, LoopOutcome::Completed);
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
    assert!(
        matches!(&provider.requests()[1].messages[2], Message::ToolResult(output) if output.is_error && output.content.contains("no human"))
    );
}

struct Approver {
    approval: Approval,
    calls: AtomicUsize,
}
#[async_trait]
impl Interaction for Approver {
    async fn confirm(&self, request: &ApprovalRequest) -> Result<Approval, String> {
        assert_eq!(request.reason, "review");
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.approval.clone())
    }
}

#[tokio::test]
async fn allow_once_and_session_approvals_have_different_scopes() {
    for (approval, confirmations, executions) in [
        (Approval::AllowOnce, 2, 2),
        (Approval::AllowSession, 1, 2),
        (Approval::Deny("declined".into()), 2, 0),
    ] {
        let provider = Arc::new(FauxProvider::new([
            FauxStep::tool_calls(vec![
                ("c1", "echo", json!({"text": "same"})),
                ("c2", "echo", json!({"text": "same"})),
            ]),
            FauxStep::text("done"),
        ]));
        let tool = Arc::new(EchoTool::new("echo"));
        let host = Arc::new(Approver {
            approval,
            calls: AtomicUsize::new(0),
        });
        let mut config = config(provider, vec![tool.clone()]);
        config.hooks.push(Verdict(Decision::Ask("review".into())));
        config.interaction = host.clone();
        let (result, _) = run_prompt(&config, &mut Vec::new(), "go").await;
        assert_eq!(result.outcome, LoopOutcome::Completed);
        assert_eq!(host.calls.load(Ordering::SeqCst), confirmations);
        assert_eq!(tool.calls.load(Ordering::SeqCst), executions);
    }
}

struct SwitchPolicy(Arc<AtomicBool>);
#[async_trait]
impl Hooks for SwitchPolicy {
    async fn before_tool(&self, _call: &ToolCall) -> HookResult<Decision> {
        Ok(if self.0.load(Ordering::SeqCst) {
            Decision::Deny("revoked".into())
        } else {
            Decision::Ask("review".into())
        })
    }
}

#[tokio::test]
async fn session_grants_require_exact_arguments_can_be_cleared_and_never_override_deny() {
    let steps = (0..5).flat_map(|index| {
        [
            FauxStep::tool_call(
                &format!("c{index}"),
                "echo",
                json!({"text": if index == 2 { "different" } else { "same" }}),
            ),
            FauxStep::text("done"),
        ]
    });
    let provider = Arc::new(FauxProvider::new(steps));
    let tool = Arc::new(EchoTool::new("echo"));
    let denied = Arc::new(AtomicBool::new(false));
    let host = Arc::new(Approver {
        approval: Approval::AllowSession,
        calls: AtomicUsize::new(0),
    });
    let mut config = config(provider, vec![tool.clone()]);
    config.interaction = host.clone();
    config.hooks.push(SwitchPolicy(denied.clone()));
    let (mut runner, _) = RunnerSetup::new(config).build();
    for _ in 0..3 {
        assert_eq!(
            runner.run("go", &CancellationToken::new()).await.outcome,
            RunOutcome::Completed
        );
    }
    assert_eq!(host.calls.load(Ordering::SeqCst), 2, "different args must ask again");
    runner.clear_approvals();
    runner.run("go", &CancellationToken::new()).await;
    assert_eq!(host.calls.load(Ordering::SeqCst), 3, "revoking grants must ask again");
    denied.store(true, Ordering::SeqCst);
    runner.run("go", &CancellationToken::new()).await;
    assert_eq!(host.calls.load(Ordering::SeqCst), 3, "deny must bypass approvals");
    assert_eq!(tool.calls.load(Ordering::SeqCst), 4);
}

enum HostFailure {
    Error,
    Panic,
    Pending,
}
#[async_trait]
impl Interaction for HostFailure {
    async fn confirm(&self, _request: &ApprovalRequest) -> Result<Approval, String> {
        match self {
            Self::Error => Err("host unavailable".into()),
            Self::Panic => panic!("host bug"),
            Self::Pending => std::future::pending().await,
        }
    }
}

#[tokio::test]
async fn approval_errors_panics_and_timeouts_block_execution_and_preserve_the_loop() {
    for (host, expected) in [
        (HostFailure::Error, "host unavailable"),
        (HostFailure::Panic, "panicked"),
        (HostFailure::Pending, "timed out"),
    ] {
        let provider = Arc::new(FauxProvider::new([
            FauxStep::tool_call("c", "echo", json!({})),
            FauxStep::text("handled"),
        ]));
        let tool = Arc::new(EchoTool::new("echo"));
        let mut config = config(provider.clone(), vec![tool.clone()]);
        config.hooks.push(Verdict(Decision::Ask("review".into())));
        config.interaction = Arc::new(host);
        config.interaction_timeout = Duration::ZERO;
        let (result, _) = run_prompt(&config, &mut Vec::new(), "go").await;
        assert_eq!(result.outcome, LoopOutcome::Completed);
        assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
        assert!(
            matches!(&provider.requests()[1].messages[2], Message::ToolResult(output) if output.is_error && output.content.contains(expected))
        );
    }
}

#[test]
fn host_approvals_wait_two_minutes_by_default() {
    let config = config(Arc::new(FauxProvider::new([])), vec![]);
    assert_eq!(config.interaction_timeout, Duration::from_secs(120));
}

struct WaitingHost(Arc<tokio::sync::Notify>);
#[async_trait]
impl Interaction for WaitingHost {
    async fn confirm(&self, _request: &ApprovalRequest) -> Result<Approval, String> {
        self.0.notify_one();
        std::future::pending().await
    }
}

#[tokio::test]
async fn cancellation_interrupts_a_pending_approval() {
    let provider = Arc::new(FauxProvider::new([FauxStep::tool_call("c", "echo", json!({}))]));
    let entered = Arc::new(tokio::sync::Notify::new());
    let tool = Arc::new(EchoTool::new("echo"));
    let mut config = config(provider, vec![tool.clone()]);
    config.hooks.push(Verdict(Decision::Ask("review".into())));
    config.interaction = Arc::new(WaitingHost(entered.clone()));
    let (mut runner, _) = RunnerSetup::new(config).build();
    let cancel = CancellationToken::new();
    let mut running = Box::pin(runner.run("go", &cancel));
    tokio::select! { _ = entered.notified() => {}, _ = &mut running => panic!("must wait for approval") }
    cancel.cancel();
    let report = tokio::time::timeout(Duration::from_secs(1), running).await.unwrap();
    assert_eq!(report.outcome, RunOutcome::Aborted);
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
}

struct CountBefore(Arc<AtomicUsize>);
#[async_trait]
impl Hooks for CountBefore {
    async fn before_tool(&self, _call: &ToolCall) -> HookResult<Decision> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Decision::Ask("review".into()))
    }
}

#[tokio::test]
async fn invalid_tool_arguments_are_rejected_before_policy_and_approval() {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c", "bash", json!({"command": "echo no", "extra": true})),
        FauxStep::text("handled"),
    ]));
    let checks = Arc::new(AtomicUsize::new(0));
    let host = Arc::new(Approver {
        approval: Approval::AllowOnce,
        calls: AtomicUsize::new(0),
    });
    let mut config = config(provider.clone(), vec![Arc::new(BashTool::default())]);
    config.hooks.push(CountBefore(checks.clone()));
    config.interaction = host.clone();
    run_prompt(&config, &mut Vec::new(), "go").await;
    assert_eq!(checks.load(Ordering::SeqCst), 0);
    assert_eq!(host.calls.load(Ordering::SeqCst), 0);
    assert!(
        matches!(&provider.requests()[1].messages[2], Message::ToolResult(output) if output.is_error && output.content.contains("unknown field"))
    );
}

struct RewriteInput;
#[async_trait]
impl Hooks for RewriteInput {
    async fn on_input(&self, text: String) -> HookResult<String> {
        Ok(text.to_uppercase())
    }
}

struct Project;
#[async_trait]
impl Hooks for Project {
    async fn transform_context(&self, messages: &[Message]) -> HookResult<Vec<ContextEdit>> {
        assert_eq!(summarize(messages), ["user:old", "user:GO"]);
        Ok(vec![
            ContextEdit::Omit { index: 0 },
            ContextEdit::ReplaceText {
                index: 0,
                text: "view:GO".into(),
            },
            ContextEdit::Append(Message::System(SystemMessage {
                sections: vec![SectionChange {
                    name: "extension".into(),
                    content: Some("logged".into()),
                }],
                timestamp: 0,
            })),
        ])
    }
}

struct AuditProbe {
    provider: Arc<FauxProvider>,
    path: PathBuf,
}
impl Provider for AuditProbe {
    fn name(&self) -> &str {
        "audit-probe"
    }
    fn stream(&self, request: Request, cancel: CancellationToken) -> EventReceiver {
        let persisted = SessionStore::open(&self.path).unwrap();
        assert_eq!(
            request.messages,
            persisted.messages(),
            "projection must be committed before provider.stream"
        );
        self.provider.stream(request, cancel)
    }
}

#[tokio::test]
async fn input_and_context_changes_are_persisted_before_the_model_request() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut session = SessionStore::create(&path, header()).unwrap();
    session.append_message(Message::user("old")).unwrap();
    let provider = Arc::new(FauxProvider::new([FauxStep::text("done")]));
    let probe = Arc::new(AuditProbe {
        provider: provider.clone(),
        path: path.clone(),
    });
    let mut config = config(probe, vec![]);
    config.hooks = HookChain::new().with(RewriteInput).with(Project);
    let (mut runner, _) = RunnerSetup {
        session,
        ..RunnerSetup::new(config)
    }
    .build();
    let report = runner.run("go", &CancellationToken::new()).await;
    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(report.final_text, "done");
    assert_eq!(
        summarize(&provider.requests()[0].messages),
        ["user:view:GO", "system:extension"]
    );
    let lines = read_lines(&path);
    assert_eq!(lines[2]["message"]["content"], "go", "original input remains intact");
    assert_eq!(lines.iter().filter(|line| line["type"] == "context_edit").count(), 3);
    assert_eq!(
        SessionStore::open(&path).unwrap().messages(),
        runner.session().messages()
    );
}

struct InvalidPlan;
#[async_trait]
impl Hooks for InvalidPlan {
    async fn transform_context(&self, _messages: &[Message]) -> HookResult<Vec<ContextEdit>> {
        Ok(vec![
            ContextEdit::Omit { index: 0 },
            ContextEdit::ReplaceText {
                index: 99,
                text: "invalid".into(),
            },
        ])
    }
}

#[tokio::test]
async fn an_invalid_edit_plan_is_rejected_before_any_partial_edit_or_request() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("never")]));
    let mut config = config(provider.clone(), vec![]);
    config.hooks.push(InvalidPlan);
    let (mut runner, _) = RunnerSetup::new(config).build();
    let report = runner.run("go", &CancellationToken::new()).await;
    assert_eq!(report.outcome, RunOutcome::Error);
    assert!(report.error.unwrap().contains("out of bounds"));
    assert!(provider.requests().is_empty());
    assert_eq!(summarize(&runner.session().messages()), ["user:go"]);
    assert!(
        !runner
            .session()
            .entries()
            .iter()
            .any(|entry| matches!(entry, Entry::ContextEdit(_)))
    );
}

struct RejectInput;
#[async_trait]
impl Hooks for RejectInput {
    async fn on_input(&self, _text: String) -> HookResult<String> {
        Err("unsupported input".into())
    }
}

#[tokio::test]
async fn rejected_input_is_logged_and_omitted_without_retrying_the_model() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("never")]));
    let mut config = config(provider.clone(), vec![]);
    config.hooks.push(RejectInput);
    let (mut runner, _) = RunnerSetup {
        retry: fast_retry(3),
        ..RunnerSetup::new(config)
    }
    .build();
    let report = runner.run("go", &CancellationToken::new()).await;
    assert_eq!(
        (report.outcome, report.turns, report.retries),
        (RunOutcome::Error, 0, 0)
    );
    assert!(provider.requests().is_empty());
    assert_eq!(runner.session().entries().len(), 2);
    assert!(runner.session().messages().is_empty());
}

#[tokio::test]
async fn steering_queued_after_a_rejected_message_stays_queued_for_the_next_run() {
    struct RejectMarked;
    #[async_trait]
    impl Hooks for RejectMarked {
        async fn on_input(&self, text: String) -> HookResult<String> {
            if text == "reject-me" {
                Err("rejected".into())
            } else {
                Ok(text)
            }
        }
    }
    let provider = Arc::new(FauxProvider::new([FauxStep::text("answer")]));
    let mut config = config(provider.clone(), vec![]);
    config.hooks.push(RejectMarked);
    let (mut runner, _) = RunnerSetup::new(config).build();
    runner.config().queues.steer("reject-me");
    runner.config().queues.steer("keep-me");
    let first = runner.run("hello", &CancellationToken::new()).await;
    assert_eq!(first.outcome, RunOutcome::Error, "{first:?}");
    let second = runner.run("again", &CancellationToken::new()).await;
    assert_eq!(second.outcome, RunOutcome::Completed, "{second:?}");
    assert_eq!(
        summarize(&provider.requests()[0].messages),
        ["user:hello", "user:again", "user:keep-me"]
    );
}

struct OmitEarlier;
#[async_trait]
impl Hooks for OmitEarlier {
    async fn transform_context(&self, messages: &[Message]) -> HookResult<Vec<ContextEdit>> {
        Ok((0..messages.len().saturating_sub(1))
            .map(|_| ContextEdit::Omit { index: 0 })
            .collect())
    }
}

#[tokio::test]
async fn omitting_prior_runs_does_not_lose_the_current_answer_or_usage() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("current")]));
    let mut session = SessionStore::in_memory(header());
    for _ in 0..5 {
        session.append_message(Message::user("old")).unwrap();
    }
    let mut config = config(provider.clone(), vec![]);
    config.hooks.push(OmitEarlier);
    let (mut runner, _) = RunnerSetup {
        session,
        ..RunnerSetup::new(config)
    }
    .build();
    let report = runner.run("go", &CancellationToken::new()).await;
    assert_eq!(report.final_text, "current");
    assert_eq!(report.usage.total_tokens, 15);
    assert_eq!(summarize(&provider.requests()[0].messages), ["user:go"]);
}

enum BadOutput {
    Error,
    Panic,
    Pending,
}
#[async_trait]
impl Hooks for BadOutput {
    async fn after_tool(&self, _call: &ToolCall, _output: ToolOutput) -> HookResult<ToolOutput> {
        match self {
            Self::Error => Err("filter rejected".into()),
            Self::Panic => panic!("filter bug"),
            Self::Pending => std::future::pending().await,
        }
    }
}

#[tokio::test]
async fn failed_output_hooks_withhold_raw_results_from_events_and_persistence() {
    for hook in [BadOutput::Error, BadOutput::Panic, BadOutput::Pending] {
        let provider = Arc::new(FauxProvider::new([
            FauxStep::tool_call("c", "echo", json!({})),
            FauxStep::text("handled"),
        ]));
        struct SecretTool;
        #[async_trait]
        impl zx_agent::agent::Tool for SecretTool {
            fn name(&self) -> &str {
                "echo"
            }
            fn definition(&self) -> zx_agent::ai::ToolDef {
                zx_agent::ai::ToolDef {
                    name: "echo".into(),
                    description: "secret".into(),
                    parameters: json!({}),
                }
            }
            async fn execute(
                &self,
                _args: serde_json::Value,
                _ctx: &zx_agent::agent::ToolContext,
            ) -> Result<ToolOutput, String> {
                Ok(ToolOutput::ok("raw-tool-secret"))
            }
        }
        let mut config = config(provider.clone(), vec![Arc::new(SecretTool)]);
        config.hooks.push_with_timeout(hook, Duration::ZERO);
        let (mut runner, events) = RunnerSetup::new(config).build();
        let report = runner.run("go", &CancellationToken::new()).await;
        assert_eq!(report.outcome, RunOutcome::Completed);
        let serialized = serde_json::to_string(runner.session().entries()).unwrap();
        assert!(!serialized.contains("raw-tool-secret"));
        assert!(
            !serde_json::to_string(&events.events())
                .unwrap()
                .contains("raw-tool-secret")
        );
        assert!(
            matches!(&provider.requests()[1].messages[2], Message::ToolResult(output) if output.is_error && output.content.contains("withheld"))
        );
    }
}

struct Observer {
    seen: Arc<Mutex<Vec<&'static str>>>,
}
#[async_trait]
impl Hooks for Observer {
    async fn on_event(&self, event: &AgentEvent) -> HookResult<()> {
        assert!(
            !matches!(event, AgentEvent::HookFailed { .. }),
            "diagnostics must not recurse"
        );
        self.seen.lock().unwrap().push(event.kind());
        Err("observer unavailable".into())
    }
}

#[tokio::test]
async fn observer_failures_are_diagnostic_and_retry_events_are_observed_once() {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::Fail("HTTP 503: retry".into()),
        FauxStep::text("done"),
    ]));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut config = config(provider, vec![]);
    config.hooks.push(Observer { seen: seen.clone() });
    let (mut runner, events) = RunnerSetup {
        retry: fast_retry(1),
        ..RunnerSetup::new(config)
    }
    .build();
    let report = runner.run("go", &CancellationToken::new()).await;
    assert_eq!(report.outcome, RunOutcome::Completed);
    let kinds = events.kinds();
    let regular: Vec<_> = kinds.iter().copied().filter(|kind| *kind != "hook_failed").collect();
    assert_eq!(*seen.lock().unwrap(), regular);
    assert_eq!(
        kinds.iter().filter(|kind| **kind == "hook_failed").count(),
        regular.len()
    );
    assert_eq!(regular.iter().filter(|kind| **kind == "retry_scheduled").count(), 1);
}

struct CustomContinue;
#[async_trait]
impl Hooks for CustomContinue {
    async fn finish_turn(&self, turn: &TurnSummary<'_>) -> HookResult<TurnDecision> {
        Ok(if turn.response.stop_reason == StopReason::Length {
            TurnDecision::ContinueAnswer("接着回答".into())
        } else {
            TurnDecision::Default
        })
    }
}

#[tokio::test]
async fn custom_continuation_wording_still_joins_answer_parts() {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::truncated("第一部分，"),
        FauxStep::text("第二部分。"),
    ]));
    let mut config = config(provider.clone(), vec![]);
    config.hooks.push(CustomContinue);
    let (mut runner, _) = RunnerSetup::new(config).build();
    let report = runner.run("go", &CancellationToken::new()).await;
    assert_eq!(report.final_text, "第一部分，第二部分。");
    assert!(matches!(&provider.requests()[1].messages[2], Message::User { content, .. } if content == "接着回答"));
}

#[tokio::test]
async fn continuation_budget_is_not_reset_after_a_provider_retry() {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::truncated("甲"),
        FauxStep::Fail("HTTP 503".into()),
        FauxStep::truncated("乙"),
        FauxStep::text("never"),
    ]));
    let mut config = config(provider.clone(), vec![]);
    config.hooks.push(ContinueOnLength::new(1));
    let (mut runner, _) = RunnerSetup {
        retry: fast_retry(1),
        limits: Limits::default(),
        ..RunnerSetup::new(config)
    }
    .build();
    let report = runner.run("go", &CancellationToken::new()).await;
    assert_eq!(
        (report.outcome, report.turns, report.retries),
        (RunOutcome::Truncated, 3, 1)
    );
    assert_eq!(report.final_text, "甲乙");
    assert_eq!(report.usage.total_tokens, 30);
    assert_eq!(provider.requests().len(), 3);
}

#[tokio::test]
async fn redaction_handles_overlapping_secrets_unicode_and_error_outputs() {
    let hook = RedactSecrets::new(["", "token", "token-long", "令牌"].into_iter().map(str::to_string));
    let call = ToolCall {
        id: "c".into(),
        name: "echo".into(),
        arguments: json!({}),
        arguments_error: None,
    };
    let output = hook
        .after_tool(&call, ToolOutput::error("token-long，令牌 token"))
        .await
        .unwrap();
    assert_eq!(output.content, "[REDACTED]，[REDACTED] [REDACTED]");
    assert!(output.is_error);
}

fn redaction_call() -> ToolCall {
    ToolCall {
        id: "c".into(),
        name: "echo".into(),
        arguments: json!({}),
        arguments_error: None,
    }
}

#[tokio::test]
async fn secret_files_are_read_again_and_earlier_values_stay_redacted() {
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    std::fs::write(&token, "first-token\n").unwrap();
    let hook = RedactSecrets::new(Vec::new()).with_secret_files([token.clone()]);
    let output = hook
        .after_tool(&redaction_call(), ToolOutput::ok("a first-token b"))
        .await
        .unwrap();
    assert_eq!(output.content, "a [REDACTED] b");
    // A rotated token: the new value is read, and the old one, still valid, stays hidden.
    std::fs::write(&token, "second-token").unwrap();
    let output = hook
        .after_tool(&redaction_call(), ToolOutput::ok("first-token second-token"))
        .await
        .unwrap();
    assert_eq!(output.content, "[REDACTED] [REDACTED]");
}

#[tokio::test]
async fn missing_or_empty_secret_files_redact_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let empty = dir.path().join("empty");
    std::fs::write(&empty, " \n").unwrap();
    let hook = RedactSecrets::new(["key".to_string()]).with_secret_files([dir.path().join("absent"), empty]);
    let output = hook
        .after_tool(&redaction_call(), ToolOutput::ok("key stays out, the rest stays"))
        .await
        .unwrap();
    assert_eq!(output.content, "[REDACTED] stays out, the rest stays");
}

#[tokio::test]
async fn a_secret_file_that_cannot_be_read_as_text_fails_the_hook() {
    let dir = tempfile::tempdir().unwrap();
    let hook = RedactSecrets::new(Vec::new()).with_secret_files([dir.path().to_path_buf()]);
    let error = hook
        .after_tool(&redaction_call(), ToolOutput::ok("output"))
        .await
        .unwrap_err();
    assert!(error.contains("cannot read a secret file"), "{error}");
}

struct NextTurn(TurnDecision);
#[async_trait]
impl Hooks for NextTurn {
    async fn finish_turn(&self, _turn: &TurnSummary<'_>) -> HookResult<TurnDecision> {
        Ok(self.0.clone())
    }
}

#[tokio::test]
async fn a_hook_follow_up_obeys_the_turn_budget() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("first"), FauxStep::text("never")]));
    let mut config = config(provider.clone(), vec![]);
    config.hooks.push(NextTurn(TurnDecision::FollowUp("next".into())));
    let (mut runner, _) = RunnerSetup {
        limits: Limits {
            max_turns: Some(1),
            timeout: None,
        },
        ..RunnerSetup::new(config)
    }
    .build();
    let report = runner.run("go", &CancellationToken::new()).await;
    assert_eq!(report.outcome, RunOutcome::MaxTurns);
    assert_eq!(provider.requests().len(), 1);
}

struct StopAndQueue(Arc<zx_agent::agent::MessageQueues>);
#[async_trait]
impl Hooks for StopAndQueue {
    async fn on_event(&self, event: &AgentEvent) -> HookResult<()> {
        if matches!(event, AgentEvent::TurnEnd { .. }) {
            self.0.steer("pending user input");
        }
        Ok(())
    }
    async fn finish_turn(&self, _turn: &TurnSummary<'_>) -> HookResult<TurnDecision> {
        Ok(TurnDecision::Stop)
    }
}

#[tokio::test]
async fn stopping_from_a_hook_preserves_queued_user_input() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("done")]));
    let mut config = config(provider, vec![]);
    config.hooks.push(StopAndQueue(config.queues.clone()));
    let (result, _) = run_prompt(&config, &mut Vec::new(), "go").await;
    assert_eq!(result.outcome, LoopOutcome::Completed);
    assert_eq!(summarize(&config.queues.drain_steering()), ["user:pending user input"]);
}

struct ContinueWithSystemUpdate;
#[async_trait]
impl Hooks for ContinueWithSystemUpdate {
    async fn transform_context(&self, _messages: &[Message]) -> HookResult<Vec<ContextEdit>> {
        Ok(vec![ContextEdit::Append(Message::System(SystemMessage {
            sections: vec![SectionChange {
                name: "extension".into(),
                content: Some("keep answering".into()),
            }],
            timestamp: 0,
        }))])
    }
    async fn finish_turn(&self, turn: &TurnSummary<'_>) -> HookResult<TurnDecision> {
        Ok(if turn.turn == 1 {
            TurnDecision::ContinueAnswer("continue".into())
        } else {
            TurnDecision::Default
        })
    }
}

#[tokio::test]
async fn marked_continuations_join_across_system_updates() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("甲"), FauxStep::text("乙")]));
    let mut config = config(provider, vec![]);
    config.hooks.push(ContinueWithSystemUpdate);
    let (mut runner, _) = RunnerSetup::new(config).build();
    let report = runner.run("go", &CancellationToken::new()).await;
    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(report.final_text, "甲乙");
}

struct FinishFailure;
#[async_trait]
impl Hooks for FinishFailure {
    async fn finish_turn(&self, _turn: &TurnSummary<'_>) -> HookResult<TurnDecision> {
        Err("HTTP 503 from extension".into())
    }
}

#[tokio::test]
async fn a_finish_hook_error_never_triggers_provider_retry() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("answer"), FauxStep::text("never")]));
    let mut config = config(provider.clone(), vec![]);
    config.hooks.push(FinishFailure);
    let (mut runner, _) = RunnerSetup {
        retry: fast_retry(3),
        ..RunnerSetup::new(config)
    }
    .build();
    let report = runner.run("go", &CancellationToken::new()).await;
    assert_eq!(
        (report.outcome, report.turns, report.retries),
        (RunOutcome::Error, 1, 0)
    );
    assert!(report.error.unwrap().contains("HTTP 503 from extension"));
    assert_eq!(provider.requests().len(), 1);
}
