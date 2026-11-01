//! Hook, approval and audit contracts, exercised through the public SDK.
mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use common::{EchoTool, RunnerSetup, config, fast_retry, header, read_lines, run_prompt, summarize};
use zx_agent::agent::{
    AgentEvent, ApprovalResult, CancelSignal, ContextEdit, Decision, Event, HookChain, HookContext, HookOptions,
    HookResult, HookStage, Hooks, LoopOutcome, Tool, ToolContext, ToolOutput, ToolVerdict, TurnDecision, TurnSummary,
};
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::{EventReceiver, Message, Provider, Request, SectionChange, StopReason, SystemMessage, ToolCall};
use zx_agent::extensions::{ContinueOnLength, RedactSecrets, check_secret_files};
use zx_agent::interaction::{Approval, ApprovalRequest, Interaction};
use zx_agent::runner::{Limits, RunOutcome};
use zx_agent::session::{Entry, SessionStore};
use zx_agent::tools::BashTool;

struct Verdict(Decision);
#[async_trait]
impl Hooks for Verdict {
    async fn before_tool(&self, _call: &ToolCall, _ctx: &HookContext) -> HookResult<Decision> {
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
    async fn before_tool(&self, _call: &ToolCall, _ctx: &HookContext) -> HookResult<Decision> {
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
    async fn before_tool(&self, _call: &ToolCall, _ctx: &HookContext) -> HookResult<Decision> {
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
    async fn on_input(&self, text: String, _ctx: &HookContext) -> HookResult<String> {
        Ok(text.to_uppercase())
    }
}

struct Project;
#[async_trait]
impl Hooks for Project {
    async fn transform_context(&self, messages: &[Message], _ctx: &HookContext) -> HookResult<Vec<ContextEdit>> {
        assert_eq!(summarize(messages), ["user:old", "user:GO"]);
        Ok(vec![
            ContextEdit::Omit { index: 0 },
            ContextEdit::ReplaceText {
                index: 0,
                text: "view:GO".into(),
            },
            ContextEdit::Append {
                message: Message::user("extension note"),
            },
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
        let persisted = SessionStore::read(&self.path).unwrap();
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
    session.append_message(Message::user("old")).await.unwrap();
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
        ["user:view:GO", "user:extension note"]
    );
    let lines = read_lines(&path);
    assert_eq!(lines[2]["message"]["content"], "go", "original input remains intact");
    assert_eq!(lines.iter().filter(|line| line["type"] == "context_edit").count(), 3);
    assert_eq!(
        SessionStore::read(&path).unwrap().messages(),
        runner.session().messages()
    );
}

struct InvalidPlan;
#[async_trait]
impl Hooks for InvalidPlan {
    async fn transform_context(&self, _messages: &[Message], _ctx: &HookContext) -> HookResult<Vec<ContextEdit>> {
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
    async fn on_input(&self, _text: String, _ctx: &HookContext) -> HookResult<String> {
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
        async fn on_input(&self, text: String, _ctx: &HookContext) -> HookResult<String> {
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
    async fn transform_context(&self, messages: &[Message], _ctx: &HookContext) -> HookResult<Vec<ContextEdit>> {
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
        session.append_message(Message::user("old")).await.unwrap();
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
    async fn after_tool(&self, _call: &ToolCall, _output: ToolOutput, _ctx: &HookContext) -> HookResult<ToolOutput> {
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
        config.hooks.push_with(
            hook,
            HookOptions {
                timeout: Duration::ZERO,
                ..HookOptions::default()
            },
        );
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
    async fn on_event(&self, event: &Event, _ctx: &HookContext) -> HookResult<()> {
        assert!(
            !matches!(event.event, AgentEvent::HookFailed { .. }),
            "diagnostics must not recurse"
        );
        self.seen.lock().unwrap().push(event.event.kind());
        Err("observer unavailable".into())
    }

    fn observes(&self, _event: &AgentEvent) -> bool {
        true
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
    async fn finish_turn(&self, turn: &TurnSummary<'_>, _ctx: &HookContext) -> HookResult<TurnDecision> {
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
        .after_tool(
            &call,
            ToolOutput::error("token-long，令牌 token"),
            &common::hook_context(),
        )
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
        .after_tool(
            &redaction_call(),
            ToolOutput::ok("a first-token b"),
            &common::hook_context(),
        )
        .await
        .unwrap();
    assert_eq!(output.content, "a [REDACTED] b");
    // A rotated token: the new value is read, and the old one, still valid, stays hidden.
    std::fs::write(&token, "second-token").unwrap();
    let output = hook
        .after_tool(
            &redaction_call(),
            ToolOutput::ok("first-token second-token"),
            &common::hook_context(),
        )
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
        .after_tool(
            &redaction_call(),
            ToolOutput::ok("key stays out, the rest stays"),
            &common::hook_context(),
        )
        .await
        .unwrap();
    assert_eq!(output.content, "[REDACTED] stays out, the rest stays");
}

#[tokio::test]
async fn a_secret_file_that_is_not_utf8_is_redacted_as_tools_print_it() {
    let dir = tempfile::tempdir().unwrap();
    // "pässword" in Latin-1: the tools decode output lossily, so it prints as "p\u{FFFD}ssword".
    std::fs::write(dir.path().join("password"), b"p\xe4ssword\n").unwrap();
    let printed = BashTool::default()
        .execute(
            json!({"command": "cat password"}),
            &ToolContext {
                cwd: dir.path().to_path_buf(),
                cancel: CancellationToken::new(),
            },
        )
        .await
        .unwrap();
    assert!(printed.content.contains("p\u{FFFD}ssword"), "{}", printed.content);
    let hook = RedactSecrets::new(Vec::new()).with_secret_files([dir.path().join("password")]);

    let output = hook
        .after_tool(&redaction_call(), printed, &common::hook_context())
        .await
        .unwrap();

    assert!(output.content.starts_with("[REDACTED]\n"), "{}", output.content);
    assert!(!output.content.contains("ssword"), "{}", output.content);
}

#[tokio::test]
async fn a_secret_file_over_the_limit_fails_the_hook() {
    let dir = tempfile::tempdir().unwrap();
    let large = dir.path().join("large");
    std::fs::write(&large, "x".repeat(64 * 1024 + 1)).unwrap();
    let hook = RedactSecrets::new(Vec::new()).with_secret_files([large]);
    let error = hook
        .after_tool(&redaction_call(), ToolOutput::ok("output"), &common::hook_context())
        .await
        .unwrap_err();
    assert!(error.contains("cannot read a secret file"), "{error}");
    assert!(error.contains("exceeds 65536 bytes"), "{error}");
}

#[test]
fn secret_files_that_cannot_be_matched_are_found_before_use() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("token"), "token-a").unwrap();
    std::fs::write(dir.path().join("latin1"), b"p\xe4ssword").unwrap();
    std::fs::write(dir.path().join("bundle"), "x".repeat(64 * 1024 + 1)).unwrap();
    #[cfg(unix)]
    assert!(
        std::process::Command::new("mkfifo")
            .arg(dir.path().join("pipe"))
            .status()
            .unwrap()
            .success()
    );
    let usable = tempfile::tempdir().unwrap();
    std::fs::write(usable.path().join("token"), "token-b").unwrap();

    let error = check_secret_files(&[dir.path().to_path_buf(), dir.path().join("absent")]).unwrap_err();

    assert!(
        error.contains(&dir.path().join("bundle").display().to_string()),
        "{error}"
    );
    #[cfg(unix)]
    assert!(
        error.contains(&dir.path().join("pipe").display().to_string()),
        "{error}"
    );
    // Text and non-UTF-8 files can be matched; a missing file is skipped, as at redaction.
    assert!(
        !error.contains("token") && !error.contains("latin1") && !error.contains("absent"),
        "{error}"
    );
    assert_eq!(check_secret_files(&[usable.path().to_path_buf()]), Ok(()));
}

#[tokio::test]
async fn a_secret_directory_covers_its_visible_files_including_later_ones() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("cluster-a"), "token-a\n").unwrap();
    std::fs::write(dir.path().join(".hidden"), "token-hidden").unwrap();
    std::fs::create_dir(dir.path().join("nested")).unwrap();
    std::fs::write(dir.path().join("nested").join("cluster-x"), "token-x").unwrap();
    let hook = RedactSecrets::new(Vec::new()).with_secret_files([dir.path().to_path_buf()]);
    let output = hook
        .after_tool(
            &redaction_call(),
            ToolOutput::ok("token-a token-hidden token-x"),
            &common::hook_context(),
        )
        .await
        .unwrap();
    assert_eq!(output.content, "[REDACTED] token-hidden token-x");
    // A key added to a Secret volume later shows up as a new file in the same directory.
    std::fs::write(dir.path().join("cluster-b"), "token-b").unwrap();
    let output = hook
        .after_tool(&redaction_call(), ToolOutput::ok("token-b"), &common::hook_context())
        .await
        .unwrap();
    assert_eq!(output.content, "[REDACTED]");
}

#[tokio::test]
async fn every_current_secret_is_redacted_however_many_files_there_are() {
    let dir = tempfile::tempdir().unwrap();
    let count = 40;
    for index in 0..count {
        std::fs::write(
            dir.path().join(format!("cluster-{index:02}")),
            format!("token-{index:02}"),
        )
        .unwrap();
    }
    let hook = RedactSecrets::new(Vec::new()).with_secret_files([dir.path().to_path_buf()]);
    let all: Vec<String> = (0..count).map(|index| format!("token-{index:02}")).collect();
    let output = hook
        .after_tool(
            &redaction_call(),
            ToolOutput::ok(all.join(" ")),
            &common::hook_context(),
        )
        .await
        .unwrap();
    assert_eq!(output.content, vec!["[REDACTED]"; count].join(" "));
}

struct NextTurn(TurnDecision);
#[async_trait]
impl Hooks for NextTurn {
    async fn finish_turn(&self, _turn: &TurnSummary<'_>, _ctx: &HookContext) -> HookResult<TurnDecision> {
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
    async fn on_event(&self, event: &Event, _ctx: &HookContext) -> HookResult<()> {
        if matches!(event.event, AgentEvent::TurnEnd { .. }) {
            self.0.steer("pending user input");
        }
        Ok(())
    }
    async fn finish_turn(&self, _turn: &TurnSummary<'_>, _ctx: &HookContext) -> HookResult<TurnDecision> {
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

struct FinishFailure;
#[async_trait]
impl Hooks for FinishFailure {
    async fn finish_turn(&self, _turn: &TurnSummary<'_>, _ctx: &HookContext) -> HookResult<TurnDecision> {
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

/// Holds input that starts with "hold" until the run is cancelled or dropped, unless released,
/// and reports when it starts holding.
struct HoldInput {
    holding: Arc<tokio::sync::Notify>,
    released: Arc<AtomicBool>,
}
#[async_trait]
impl Hooks for HoldInput {
    async fn on_input(&self, text: String, _ctx: &HookContext) -> HookResult<String> {
        if text.starts_with("hold") && !self.released.load(Ordering::SeqCst) {
            self.holding.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(text)
    }
}

fn holding_runner(provider: Arc<FauxProvider>) -> (zx_agent::runner::Runner, HoldInput) {
    let holding = Arc::new(tokio::sync::Notify::new());
    let released = Arc::new(AtomicBool::new(false));
    let mut config = config(provider, vec![]);
    config.hooks.push(HoldInput {
        holding: holding.clone(),
        released: released.clone(),
    });
    let (runner, _) = RunnerSetup::new(config).build();
    (runner, HoldInput { holding, released })
}

fn mentions(messages: &[Message], text: &str) -> usize {
    summarize(messages)
        .iter()
        .filter(|message| message.contains(text))
        .count()
}

#[tokio::test]
async fn input_whose_hook_was_cancelled_is_not_recorded() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("done")]));
    let (mut runner, hold) = holding_runner(provider.clone());
    let cancel = CancellationToken::new();

    let (report, ()) = tokio::join!(runner.run("hold: rotate the logs", &cancel), async {
        hold.holding.notified().await;
        cancel.cancel();
    });

    assert_eq!(report.outcome, RunOutcome::Aborted);
    assert_eq!(mentions(&runner.session().messages(), "rotate the logs"), 0);
    let report = runner.run("next", &CancellationToken::new()).await;
    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(summarize(&provider.requests()[0].messages), ["user:next"]);
}

#[tokio::test]
async fn queued_input_whose_run_was_dropped_stays_queued_and_unrecorded() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("done")]));
    let (mut runner, hold) = holding_runner(provider.clone());
    runner.config().queues.steer("hold: check the disk");
    let cancel = CancellationToken::new();

    tokio::select! {
        _ = runner.run("go", &cancel) => panic!("the input hook should still hold the input"),
        () = hold.holding.notified() => {}
    }

    assert_eq!(mentions(&runner.session().messages(), "check the disk"), 0);
    hold.released.store(true, Ordering::SeqCst);
    let report = runner.run("next", &CancellationToken::new()).await;
    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(
        summarize(&provider.requests()[0].messages),
        ["user:go", "user:next", "user:hold: check the disk"]
    );
}

/// The policy verdicts and approvals of a run, in order, as `(call, what)` pairs.
fn approvals(events: &common::Collect) -> Vec<(String, String)> {
    events
        .events()
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::ToolDecision {
                tool_call_id, verdict, ..
            } => Some((tool_call_id, format!("{verdict:?}"))),
            AgentEvent::ApprovalRequested { tool_call_id, .. } => Some((tool_call_id, "requested".into())),
            AgentEvent::ApprovalResolved {
                tool_call_id, approval, ..
            } => Some((tool_call_id, format!("{approval:?}"))),
            _ => None,
        })
        .collect()
}

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected
        .iter()
        .map(|(call, what)| ((*call).to_string(), (*what).to_string()))
        .collect()
}

#[tokio::test]
async fn the_policy_verdict_of_every_call_is_reported_with_its_reason() {
    for (decision, verdict, reason) in [
        (Decision::Allow, ToolVerdict::Allow, None),
        (
            Decision::Deny("not on the list".into()),
            ToolVerdict::Deny,
            Some("not on the list".to_string()),
        ),
    ] {
        let provider = Arc::new(FauxProvider::new([
            FauxStep::tool_call("c1", "echo", json!({})),
            FauxStep::text("done"),
        ]));
        let mut config = config(provider, vec![Arc::new(EchoTool::new("echo"))]);
        config.hooks.push(Verdict(decision));

        let (_, events) = run_prompt(&config, &mut Vec::new(), "go").await;

        let decisions: Vec<_> = events
            .events()
            .into_iter()
            .filter_map(|event| match event {
                AgentEvent::ToolDecision { verdict, reason, .. } => Some((verdict, reason)),
                _ => None,
            })
            .collect();
        assert_eq!(decisions, [(verdict, reason)]);
        assert_eq!(approvals(&events).len(), 1, "nobody is asked");
    }
}

#[tokio::test]
async fn approvals_are_reported_when_asked_and_when_remembered() {
    for (approval, expected) in [
        (
            Approval::AllowOnce,
            pairs(&[
                ("c1", "Ask"),
                ("c1", "requested"),
                ("c1", "AllowOnce"),
                ("c2", "Ask"),
                ("c2", "requested"),
                ("c2", "AllowOnce"),
            ]),
        ),
        (
            Approval::AllowSession,
            pairs(&[
                ("c1", "Ask"),
                ("c1", "requested"),
                ("c1", "AllowSession"),
                ("c2", "Ask"),
                ("c2", "Remembered"),
            ]),
        ),
        (
            Approval::Deny("declined".into()),
            pairs(&[
                ("c1", "Ask"),
                ("c1", "requested"),
                ("c1", "Denied"),
                ("c2", "Ask"),
                ("c2", "requested"),
                ("c2", "Denied"),
            ]),
        ),
    ] {
        let provider = Arc::new(FauxProvider::new([
            FauxStep::tool_calls(vec![
                ("c1", "echo", json!({"text": "same"})),
                ("c2", "echo", json!({"text": "same"})),
            ]),
            FauxStep::text("done"),
        ]));
        let mut config = config(provider, vec![Arc::new(EchoTool::new("echo"))]);
        config.hooks.push(Verdict(Decision::Ask("review".into())));
        config.interaction = Arc::new(Approver {
            approval,
            calls: AtomicUsize::new(0),
        });

        let (_, events) = run_prompt(&config, &mut Vec::new(), "go").await;

        assert_eq!(approvals(&events), expected);
    }
}

/// Answers after `delay`, like a person who takes a moment.
struct SlowApprover(Duration);
#[async_trait]
impl Interaction for SlowApprover {
    async fn confirm(&self, _request: &ApprovalRequest) -> Result<Approval, String> {
        tokio::time::sleep(self.0).await;
        Ok(Approval::AllowOnce)
    }
}

#[tokio::test]
async fn a_resolved_approval_reports_how_long_it_waited() {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c1", "echo", json!({})),
        FauxStep::text("done"),
    ]));
    let mut config = config(provider, vec![Arc::new(EchoTool::new("echo"))]);
    config.hooks.push(Verdict(Decision::Ask("review".into())));
    config.interaction = Arc::new(SlowApprover(Duration::from_millis(20)));

    let (_, events) = run_prompt(&config, &mut Vec::new(), "go").await;

    let waited: Vec<u64> = events
        .events()
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::ApprovalResolved { wait_ms, .. } => Some(wait_ms),
            _ => None,
        })
        .collect();
    assert_eq!(waited.len(), 1);
    assert!(waited[0] >= 20, "{waited:?}");
}

#[tokio::test]
async fn approvals_that_fail_or_time_out_report_how_they_ended() {
    for (host, ended) in [
        (HostFailure::Error, ApprovalResult::Failed),
        (HostFailure::Panic, ApprovalResult::Failed),
        (HostFailure::Pending, ApprovalResult::TimedOut),
    ] {
        let provider = Arc::new(FauxProvider::new([
            FauxStep::tool_call("c", "echo", json!({})),
            FauxStep::text("handled"),
        ]));
        let mut config = config(provider, vec![Arc::new(EchoTool::new("echo"))]);
        config.hooks.push(Verdict(Decision::Ask("review".into())));
        config.interaction = Arc::new(host);
        config.interaction_timeout = Duration::ZERO;

        let (_, events) = run_prompt(&config, &mut Vec::new(), "go").await;

        let resolved: Vec<_> = events
            .events()
            .into_iter()
            .filter_map(|event| match event {
                AgentEvent::ApprovalResolved { approval, reason, .. } => Some((approval, reason.is_some())),
                _ => None,
            })
            .collect();
        assert_eq!(resolved, [(ended, true)]);
    }
}

#[tokio::test]
async fn an_approval_interrupted_by_cancellation_is_reported_as_cancelled() {
    let provider = Arc::new(FauxProvider::new([FauxStep::tool_call("c", "echo", json!({}))]));
    let entered = Arc::new(tokio::sync::Notify::new());
    let mut config = config(provider, vec![Arc::new(EchoTool::new("echo"))]);
    config.hooks.push(Verdict(Decision::Ask("review".into())));
    config.interaction = Arc::new(WaitingHost(entered.clone()));
    let (mut runner, events) = RunnerSetup::new(config).build();
    let cancel = CancellationToken::new();

    let (report, ()) = tokio::join!(runner.run("go", &cancel), async {
        entered.notified().await;
        cancel.cancel();
    });

    assert_eq!(report.outcome, RunOutcome::Aborted);
    assert_eq!(
        approvals(&events),
        pairs(&[("c", "Ask"), ("c", "requested"), ("c", "Cancelled")])
    );
}

/// Records the kinds of the events it observes; `all` also observes streaming deltas.
struct Kinds {
    seen: Arc<Mutex<Vec<&'static str>>>,
    all: bool,
}
#[async_trait]
impl Hooks for Kinds {
    async fn on_event(&self, event: &Event, _ctx: &HookContext) -> HookResult<()> {
        self.seen.lock().unwrap().push(event.event.kind());
        Ok(())
    }

    fn observes(&self, event: &AgentEvent) -> bool {
        self.all || !matches!(event, AgentEvent::MessageUpdate { .. })
    }
}

#[tokio::test]
async fn observers_skip_streaming_deltas_unless_they_ask_for_them() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("done")]));
    let mut config = config(provider, vec![]);
    let default = Arc::new(Mutex::new(Vec::new()));
    let all = Arc::new(Mutex::new(Vec::new()));
    config.hooks.push(Kinds {
        seen: default.clone(),
        all: false,
    });
    config.hooks.push(Kinds {
        seen: all.clone(),
        all: true,
    });

    let (_, events) = run_prompt(&config, &mut Vec::new(), "go").await;

    let kinds = events.kinds();
    assert!(kinds.contains(&"message_update"), "the provider streamed deltas");
    assert_eq!(*all.lock().unwrap(), kinds);
    let without_deltas: Vec<_> = kinds.into_iter().filter(|kind| *kind != "message_update").collect();
    assert_eq!(*default.lock().unwrap(), without_deltas);
}

/// What each hook method of a [`ContextProbe`] received: method, session, run and directory.
type Seen = Arc<Mutex<Vec<(&'static str, Option<String>, String, PathBuf)>>>;

/// Records the context each hook method receives, and keeps the run's cancel signal.
#[derive(Clone, Default)]
struct ContextProbe {
    seen: Seen,
    cancel: Arc<Mutex<Option<CancelSignal>>>,
}
impl ContextProbe {
    fn record(&self, method: &'static str, ctx: &HookContext) {
        assert!(!ctx.cancel.is_cancelled(), "{method} ran in a cancelled run");
        self.seen
            .lock()
            .unwrap()
            .push((method, ctx.session_id.clone(), ctx.run_id.clone(), ctx.cwd.clone()));
        *self.cancel.lock().unwrap() = Some(ctx.cancel.clone());
    }
}
#[async_trait]
impl Hooks for ContextProbe {
    async fn on_input(&self, text: String, ctx: &HookContext) -> HookResult<String> {
        self.record("on_input", ctx);
        Ok(text)
    }
    async fn transform_context(&self, _messages: &[Message], ctx: &HookContext) -> HookResult<Vec<ContextEdit>> {
        self.record("transform_context", ctx);
        Ok(Vec::new())
    }
    async fn before_tool(&self, _call: &ToolCall, ctx: &HookContext) -> HookResult<Decision> {
        self.record("before_tool", ctx);
        Ok(Decision::Allow)
    }
    async fn after_tool(&self, _call: &ToolCall, output: ToolOutput, ctx: &HookContext) -> HookResult<ToolOutput> {
        self.record("after_tool", ctx);
        Ok(output)
    }
    async fn finish_turn(&self, _turn: &TurnSummary<'_>, ctx: &HookContext) -> HookResult<TurnDecision> {
        self.record("finish_turn", ctx);
        Ok(TurnDecision::Default)
    }
    async fn on_event(&self, _event: &Event, ctx: &HookContext) -> HookResult<()> {
        self.record("on_event", ctx);
        Ok(())
    }
}

#[tokio::test]
async fn every_hook_method_knows_the_session_run_and_working_directory() {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c1", "echo", json!({})),
        FauxStep::text("done"),
    ]));
    let probe = ContextProbe::default();
    let mut config = config(provider, vec![Arc::new(EchoTool::new("echo"))]);
    config.spec.cwd = PathBuf::from("/srv/work");
    config.hooks.push(probe.clone());
    let (mut runner, events) = RunnerSetup::new(config).build();

    runner.run("go", &CancellationToken::new()).await;

    let session_id = runner.session().header().id.clone();
    let run_id = events.envelopes()[0].run_id.clone();
    let seen = probe.seen.lock().unwrap().clone();
    let mut methods: Vec<&str> = seen.iter().map(|(method, ..)| *method).collect();
    methods.sort_unstable();
    methods.dedup();
    assert_eq!(
        methods,
        [
            "after_tool",
            "before_tool",
            "finish_turn",
            "on_event",
            "on_input",
            "transform_context"
        ]
    );
    for (method, session, run, cwd) in &seen {
        assert_eq!(session.as_deref(), Some(session_id.as_str()), "{method}");
        assert_eq!(run, &run_id, "{method}: the run ID of the events");
        assert_eq!(cwd, Path::new("/srv/work"), "{method}");
    }
    let cancel = probe.cancel.lock().unwrap().clone().unwrap();
    assert!(cancel.is_cancelled(), "the token is the run's, which ends with the run");
}

struct FailingObserver;
#[async_trait]
impl Hooks for FailingObserver {
    async fn on_event(&self, _event: &Event, _ctx: &HookContext) -> HookResult<()> {
        Err("exporter offline".into())
    }
}

#[tokio::test]
async fn a_named_hook_is_reported_under_its_name() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("done")]));
    let mut config = config(provider, vec![]);
    config.hooks.push_with(
        FailingObserver,
        HookOptions {
            name: Some("trace-exporter".into()),
            ..HookOptions::default()
        },
    );
    let (mut runner, events) = RunnerSetup::new(config).build();

    runner.run("go", &CancellationToken::new()).await;

    let hooks: Vec<String> = events
        .events()
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::HookFailed { hook, .. } => Some(hook),
            _ => None,
        })
        .collect();
    assert!(!hooks.is_empty());
    assert!(hooks.iter().all(|hook| hook == "trace-exporter"), "{hooks:?}");
}

/// Records its tag in before_tool and after_tool; runs in the stage it declares.
struct Staged {
    stage: HookStage,
    tag: &'static str,
    order: Arc<Mutex<Vec<String>>>,
}
#[async_trait]
impl Hooks for Staged {
    async fn before_tool(&self, _call: &ToolCall, _ctx: &HookContext) -> HookResult<Decision> {
        self.order.lock().unwrap().push(format!("before:{}", self.tag));
        Ok(Decision::Allow)
    }
    async fn after_tool(&self, _call: &ToolCall, output: ToolOutput, _ctx: &HookContext) -> HookResult<ToolOutput> {
        self.order.lock().unwrap().push(format!("after:{}", self.tag));
        Ok(output)
    }
    fn stage(&self) -> HookStage {
        self.stage
    }
}

#[tokio::test]
async fn hooks_run_in_stage_order_whatever_the_registration_order() {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c1", "echo", json!({})),
        FauxStep::text("done"),
    ]));
    let order = Arc::new(Mutex::new(Vec::new()));
    let mut config = config(provider, vec![Arc::new(EchoTool::new("echo"))]);
    for (stage, tag) in [
        (HookStage::Storage, "storage"),
        (HookStage::Redaction, "redaction"),
        (HookStage::Extension, "extension-1"),
        (HookStage::Policy, "policy"),
        (HookStage::Extension, "extension-2"),
    ] {
        config.hooks.push(Staged {
            stage,
            tag,
            order: order.clone(),
        });
    }

    run_prompt(&config, &mut Vec::new(), "go").await;

    let order = order.lock().unwrap().clone();
    let expected: Vec<String> = ["policy", "extension-1", "extension-2", "redaction", "storage"]
        .iter()
        .map(|tag| format!("before:{tag}"))
        .chain(
            ["policy", "extension-1", "extension-2", "redaction", "storage"]
                .iter()
                .map(|tag| format!("after:{tag}")),
        )
        .collect();
    assert_eq!(order, expected);
}

/// An extension that copies a secret into tool output.
struct LeakSecret;
#[async_trait]
impl Hooks for LeakSecret {
    async fn after_tool(&self, _call: &ToolCall, mut output: ToolOutput, _ctx: &HookContext) -> HookResult<ToolOutput> {
        output.content.push_str(" sk-registered-early");
        Ok(output)
    }
}

#[tokio::test]
async fn redaction_registered_first_still_runs_after_extensions() {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c1", "echo", json!({"text": "ok"})),
        FauxStep::text("done"),
    ]));
    let mut config = config(provider.clone(), vec![Arc::new(EchoTool::new("echo"))]);
    config
        .hooks
        .push(RedactSecrets::new(Some("sk-registered-early".into())));
    config.hooks.push(LeakSecret);

    run_prompt(&config, &mut Vec::new(), "go").await;

    let sent = summarize(&provider.requests()[1].messages);
    assert_eq!(sent.last().unwrap(), "tool:c1=ok [REDACTED]");
}

#[tokio::test]
async fn tools_with_the_same_name_are_rejected_before_any_request() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("never sent")]));
    let config = config(
        provider.clone(),
        vec![Arc::new(EchoTool::new("echo")), Arc::new(EchoTool::new("echo"))],
    );

    let (result, _) = run_prompt(&config, &mut Vec::new(), "go").await;

    assert_eq!(result.outcome, LoopOutcome::Error);
    let error = format!("{:?}", result.failure);
    assert!(error.contains("more than one tool is named \\\"echo\\\""), "{error}");
    assert!(provider.requests().is_empty());
}

struct AppendSystem;
#[async_trait]
impl Hooks for AppendSystem {
    async fn transform_context(&self, _messages: &[Message], _ctx: &HookContext) -> HookResult<Vec<ContextEdit>> {
        Ok(vec![ContextEdit::Append {
            message: Message::System(SystemMessage {
                sections: vec![SectionChange {
                    name: "memory".into(),
                    content: Some("remembered".into()),
                }],
                timestamp: 0,
            }),
        }])
    }
}

#[tokio::test]
async fn a_hook_cannot_append_system_messages() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("never sent")]));
    let mut config = config(provider.clone(), vec![]);
    config.hooks.push(AppendSystem);

    let (result, _) = run_prompt(&config, &mut Vec::new(), "go").await;

    assert_eq!(result.outcome, LoopOutcome::Error);
    let error = format!("{:?}", result.failure);
    assert!(error.contains("system prompt comes from the configuration"), "{error}");
    assert!(provider.requests().is_empty());
}

struct PanickingObserver;
#[async_trait]
impl Hooks for PanickingObserver {
    fn observes(&self, _event: &AgentEvent) -> bool {
        panic!("observes is broken")
    }
}

#[tokio::test]
async fn a_panic_in_observes_is_reported_and_the_run_continues() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("done")]));
    let mut config = config(provider, vec![]);
    config.hooks.push(PanickingObserver);

    let (result, sink) = run_prompt(&config, &mut Vec::new(), "go").await;

    assert_eq!(result.outcome, LoopOutcome::Completed);
    assert!(sink.kinds().contains(&"hook_failed"), "{:?}", sink.kinds());
}

/// A tool that may declare itself under another name, or panic while declaring itself.
struct Declared {
    name: &'static str,
    declared: &'static str,
    panics: bool,
}

#[async_trait]
impl zx_agent::agent::Tool for Declared {
    fn name(&self) -> &str {
        self.name
    }
    fn definition(&self) -> zx_agent::ai::ToolDef {
        assert!(!self.panics, "definition is broken");
        zx_agent::ai::ToolDef {
            name: self.declared.into(),
            description: "Test tool".into(),
            parameters: json!({"type": "object"}),
        }
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &zx_agent::agent::ToolContext,
    ) -> Result<ToolOutput, String> {
        Ok(ToolOutput::ok("ran"))
    }
}

#[tokio::test]
async fn a_tool_declared_under_another_name_is_rejected_before_any_request() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("never sent")]));
    let tool = Declared {
        name: "echo",
        declared: "other",
        panics: false,
    };
    let config = config(provider.clone(), vec![Arc::new(tool)]);

    let (result, _) = run_prompt(&config, &mut Vec::new(), "go").await;

    assert_eq!(result.outcome, LoopOutcome::Error);
    let error = format!("{:?}", result.failure);
    assert!(error.contains("declares itself as \\\"other\\\""), "{error}");
    assert!(provider.requests().is_empty());
}

#[tokio::test]
async fn a_tool_whose_definition_panics_fails_the_run_before_any_request() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("never sent")]));
    let tool = Declared {
        name: "echo",
        declared: "echo",
        panics: true,
    };
    let config = config(provider.clone(), vec![Arc::new(tool)]);

    let (result, _) = run_prompt(&config, &mut Vec::new(), "go").await;

    assert_eq!(result.outcome, LoopOutcome::Error);
    let error = format!("{:?}", result.failure);
    assert!(error.contains("panicked"), "{error}");
    assert!(provider.requests().is_empty());
}

/// Cancels the token it is given, as a tool might to stop work of its own.
struct CancelsItself;

#[async_trait]
impl zx_agent::agent::Tool for CancelsItself {
    fn name(&self) -> &str {
        "stop"
    }
    fn definition(&self) -> zx_agent::ai::ToolDef {
        zx_agent::ai::ToolDef {
            name: "stop".into(),
            description: "Test tool".into(),
            parameters: json!({"type": "object"}),
        }
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        ctx: &zx_agent::agent::ToolContext,
    ) -> Result<ToolOutput, String> {
        ctx.cancel.cancel();
        Ok(ToolOutput::ok("stopped its own work"))
    }
}

#[tokio::test]
async fn a_tool_cancelling_its_token_stops_only_itself() {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c1", "stop", json!({})),
        FauxStep::text("done"),
    ]));
    let config = config(provider, vec![Arc::new(CancelsItself)]);
    let mut context = Vec::new();

    let (result, _) = run_prompt(&config, &mut context, "go").await;

    assert_eq!(result.outcome, LoopOutcome::Completed);
    assert!(
        summarize(&context).contains(&"tool:c1=stopped its own work".to_string()),
        "{:?}",
        summarize(&context)
    );
}
