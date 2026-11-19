//! Hook, approval and audit contracts, exercised through the public SDK.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use zx_agent::agent::RunLimits;
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::{
    EventReceiver, Message, Provider, ProviderErrorKind, Request, SectionChange, StopReason, SystemMessage, ToolCall,
};
use zx_agent::builtin::continuation::ContinueOnLength;
use zx_agent::builtin::redact::{RedactSecrets, check_secret_files, short_secret_files};
use zx_agent::builtin::tools::BashTool;
use zx_agent::events::{ApprovalResult, Budget, Event, EventKind, RunOutcome};
use zx_agent::hooks::{ContextEdit, FinishedStep, Hook, HookContext, HookResult, HookStage, StepDecision};
use zx_agent::input::Input;
use zx_agent::policy::{ApprovalDecision, ApprovalMode, PolicyContext, PolicyDecision, ToolPolicy};
use zx_agent::tools::{Tool, ToolContext, ToolOutput};
use zx_agent::transcript::Entry;
use zx_agent::{CancelSignal, SessionOptions};
use zx_testing::{
    Asked, EchoTool, RunnerSetup, Seed, Start, config, fast_retry, header, read_lines, refusal, run_approved,
    run_prompt, summarize, take_queued_inputs,
};

struct Verdict(PolicyDecision);
#[async_trait]
impl ToolPolicy for Verdict {
    async fn check(&self, _call: &ToolCall, _ctx: &PolicyContext) -> PolicyDecision {
        self.0.clone()
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
    config.policies.push(
        "verdict",
        Arc::new(Verdict(PolicyDecision::Ask("review required".into()))),
    );
    config
        .policies
        .push("verdict", Arc::new(Verdict(PolicyDecision::Allow)));
    let (result, _) = run_prompt(config, &mut Vec::new(), "go").await;
    assert_eq!(result.outcome, RunOutcome::Completed);
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
    assert!(
        matches!(&provider.requests()[1].messages[2], Message::ToolResult(output) if output.is_error && output.content.contains("no human"))
    );
}

struct Approver {
    approval: ApprovalDecision,
    calls: AtomicUsize,
}
#[async_trait]
impl zx_testing::Approver for Approver {
    async fn confirm(&self, request: &Asked) -> Option<ApprovalDecision> {
        assert_eq!(request.reason, "review");
        self.calls.fetch_add(1, Ordering::SeqCst);
        Some(self.approval.clone())
    }
}

#[tokio::test]
async fn allow_once_and_session_approvals_have_different_scopes() {
    for (approval, confirmations, executions) in [
        (ApprovalDecision::AllowOnce, 2, 2),
        (ApprovalDecision::AllowSession, 1, 2),
        (ApprovalDecision::Deny("declined".into()), 2, 0),
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
        config
            .policies
            .push("verdict", Arc::new(Verdict(PolicyDecision::Ask("review".into()))));
        let (result, _) = run_approved(config, host.clone(), "go").await;
        assert_eq!(result.outcome, RunOutcome::Completed);
        assert_eq!(host.calls.load(Ordering::SeqCst), confirmations);
        assert_eq!(tool.calls.load(Ordering::SeqCst), executions);
    }
}

struct SwitchPolicy(Arc<AtomicBool>);
#[async_trait]
impl ToolPolicy for SwitchPolicy {
    async fn check(&self, _call: &ToolCall, _ctx: &PolicyContext) -> PolicyDecision {
        if self.0.load(Ordering::SeqCst) {
            PolicyDecision::Deny("revoked".into())
        } else {
            PolicyDecision::Ask("review".into())
        }
    }
}

#[tokio::test]
async fn session_grants_require_exact_arguments_and_never_override_deny() {
    let steps = (0..4).flat_map(|index| {
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
        approval: ApprovalDecision::AllowSession,
        calls: AtomicUsize::new(0),
    });
    let mut config = config(provider, vec![tool.clone()]);
    config.policies.push("switch", Arc::new(SwitchPolicy(denied.clone())));
    let (runner, _) = RunnerSetup {
        approver: Some(host.clone()),
        ..RunnerSetup::new(config)
    }
    .build()
    .await;
    for _ in 0..3 {
        assert_eq!(runner.run("go").await.outcome, RunOutcome::Completed);
    }
    assert_eq!(host.calls.load(Ordering::SeqCst), 2, "different args must ask again");
    denied.store(true, Ordering::SeqCst);
    runner.run("go").await;
    assert_eq!(host.calls.load(Ordering::SeqCst), 2, "deny must bypass approvals");
    assert_eq!(tool.calls.load(Ordering::SeqCst), 3);
}

/// Is asked and never answers; says when it was asked.
struct WaitingHost(Arc<tokio::sync::Notify>);
#[async_trait]
impl zx_testing::Approver for WaitingHost {
    async fn confirm(&self, _request: &Asked) -> Option<ApprovalDecision> {
        self.0.notify_one();
        std::future::pending().await
    }
}

#[tokio::test]
async fn an_approval_nobody_answers_in_time_blocks_the_call_and_the_loop_goes_on() {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c", "echo", json!({})),
        FauxStep::text("handled"),
    ]));
    let tool = Arc::new(EchoTool::new("echo"));
    let mut config = config(provider.clone(), vec![tool.clone()]);
    config
        .policies
        .push("verdict", Arc::new(Verdict(PolicyDecision::Ask("review".into()))));
    config.approvals = ApprovalMode::Wait(Duration::ZERO);
    let host = Arc::new(WaitingHost(Arc::default()));
    let (result, _) = run_approved(config, host, "go").await;
    assert_eq!(result.outcome, RunOutcome::Completed);
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
    assert!(
        matches!(&provider.requests()[1].messages[2], Message::ToolResult(output) if output.is_error && output.content.contains("nobody answered the approval request"))
    );
}

#[test]
fn nobody_answers_approvals_unless_the_host_says_it_does() {
    let config = config(Arc::new(FauxProvider::new([])), vec![]);
    assert_eq!(config.approvals, ApprovalMode::Deny);
}

#[tokio::test]
async fn cancellation_interrupts_a_pending_approval() {
    let provider = Arc::new(FauxProvider::new([FauxStep::tool_call("c", "echo", json!({}))]));
    let entered = Arc::new(tokio::sync::Notify::new());
    let tool = Arc::new(EchoTool::new("echo"));
    let mut config = config(provider, vec![tool.clone()]);
    config
        .policies
        .push("verdict", Arc::new(Verdict(PolicyDecision::Ask("review".into()))));
    let (runner, _) = RunnerSetup {
        approver: Some(Arc::new(WaitingHost(entered.clone()))),
        ..RunnerSetup::new(config)
    }
    .build()
    .await;
    let mut running = Box::pin(runner.run("go"));
    tokio::select! { () = entered.notified() => {}, _ = &mut running => panic!("must wait for approval") }
    runner.cancel();
    let report = tokio::time::timeout(Duration::from_secs(1), running).await.unwrap();
    assert_eq!(report.outcome, RunOutcome::Cancelled);
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
}

struct CountBefore(Arc<AtomicUsize>);
#[async_trait]
impl ToolPolicy for CountBefore {
    async fn check(&self, _call: &ToolCall, _ctx: &PolicyContext) -> PolicyDecision {
        self.0.fetch_add(1, Ordering::SeqCst);
        PolicyDecision::Ask("review".into())
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
        approval: ApprovalDecision::AllowOnce,
        calls: AtomicUsize::new(0),
    });
    let mut config = config(provider.clone(), vec![Arc::new(BashTool::default())]);
    config.policies.push("count", Arc::new(CountBefore(checks.clone())));
    run_approved(config, host.clone(), "go").await;
    assert_eq!(checks.load(Ordering::SeqCst), 0);
    assert_eq!(host.calls.load(Ordering::SeqCst), 0);
    assert!(
        matches!(&provider.requests()[1].messages[2], Message::ToolResult(output) if output.is_error && output.content.starts_with("Not executed: the arguments of"))
    );
}

struct RewriteInput;
#[async_trait]
impl Hook for RewriteInput {
    async fn on_input(&self, text: String, _ctx: &HookContext) -> HookResult<String> {
        Ok(text.to_uppercase())
    }
}

struct Project;
#[async_trait]
impl Hook for Project {
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
        let persisted = zx_testing::read_session_file(&self.path).unwrap();
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
    let mut session = zx_testing::create_session_file(&path, header()).await.unwrap();
    session.append_message(Message::user("old")).await.unwrap();
    let provider = Arc::new(FauxProvider::new([FauxStep::text("done")]));
    let probe = Arc::new(AuditProbe {
        provider: provider.clone(),
        path: path.clone(),
    });
    let mut config = config(probe, vec![]);
    config.hooks.push("rewrite_input", Arc::new(RewriteInput));
    config.hooks.push("project", Arc::new(Project));
    let (runner, _) = RunnerSetup {
        session: session.into(),
        ..RunnerSetup::new(config)
    }
    .build()
    .await;
    let report = runner.run("go").await;
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
        zx_testing::read_session_file(&path).unwrap().messages(),
        runner.session().messages()
    );
}

struct InvalidPlan;
#[async_trait]
impl Hook for InvalidPlan {
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
    config.hooks.push("invalid_plan", Arc::new(InvalidPlan));
    let (runner, _) = RunnerSetup::new(config).build().await;
    let report = runner.run("go").await;
    assert_eq!(report.outcome, RunOutcome::Failed);
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
impl Hook for RejectInput {
    async fn on_input(&self, _text: String, _ctx: &HookContext) -> HookResult<String> {
        Err("unsupported input".into())
    }
}

#[tokio::test]
async fn rejected_input_is_logged_and_omitted_without_retrying_the_model() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("never")]));
    let mut config = config(provider.clone(), vec![]);
    config.hooks.push("reject_input", Arc::new(RejectInput));
    let (runner, _) = RunnerSetup {
        retry: fast_retry(3),
        ..RunnerSetup::new(config)
    }
    .build()
    .await;
    let report = runner.run("go").await;
    assert_eq!(
        (report.outcome, report.model_requests, report.retries),
        (RunOutcome::Failed, 0, 0)
    );
    assert!(provider.requests().is_empty());
    assert_eq!(runner.session().entries().len(), 2);
    assert!(runner.session().messages().is_empty());
}

#[tokio::test]
async fn steering_queued_after_a_rejected_message_stays_queued_for_the_next_run() {
    struct RejectMarked;
    #[async_trait]
    impl Hook for RejectMarked {
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
    config.hooks.push("reject_marked", Arc::new(RejectMarked));
    let (runner, _) = RunnerSetup::new(config).build().await;
    runner.queues().steer("reject-me");
    runner.queues().steer("keep-me");
    let first = runner.run("hello").await;
    assert_eq!(first.outcome, RunOutcome::Failed, "{first:?}");
    let second = runner.run("again").await;
    assert_eq!(second.outcome, RunOutcome::Completed, "{second:?}");
    assert_eq!(
        summarize(&provider.requests()[0].messages),
        ["user:hello", "user:again", "user:keep-me"]
    );
}

struct OmitEarlier;
#[async_trait]
impl Hook for OmitEarlier {
    async fn transform_context(&self, messages: &[Message], _ctx: &HookContext) -> HookResult<Vec<ContextEdit>> {
        Ok((0..messages.len().saturating_sub(1))
            .map(|_| ContextEdit::Omit { index: 0 })
            .collect())
    }
}

#[tokio::test]
async fn omitting_prior_runs_does_not_lose_the_current_answer_or_usage() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("current")]));
    let mut session = Seed::in_memory(header()).await;
    for _ in 0..5 {
        session.append_message(Message::user("old")).await.unwrap();
    }
    let mut config = config(provider.clone(), vec![]);
    config.hooks.push("omit_earlier", Arc::new(OmitEarlier));
    let (runner, _) = RunnerSetup {
        session: session.into(),
        ..RunnerSetup::new(config)
    }
    .build()
    .await;
    let report = runner.run("go").await;
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
impl Hook for BadOutput {
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
    struct SecretTool;
    #[async_trait]
    impl zx_agent::tools::Tool for SecretTool {
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
            _ctx: &zx_agent::tools::ToolContext,
        ) -> Result<ToolOutput, String> {
            Ok(ToolOutput::ok("raw-tool-secret"))
        }
    }
    for hook in [BadOutput::Error, BadOutput::Panic, BadOutput::Pending] {
        let provider = Arc::new(FauxProvider::new([
            FauxStep::tool_call("c", "echo", json!({})),
            FauxStep::text("handled"),
        ]));
        let mut config = config(provider.clone(), vec![Arc::new(SecretTool)]);
        config
            .hooks
            .push_with_timeout("bad_output", Arc::new(hook), Duration::ZERO);
        let (runner, events) = RunnerSetup::new(config).build().await;
        let report = runner.run("go").await;
        assert_eq!(report.outcome, RunOutcome::Completed);
        let serialized = serde_json::to_string(runner.session().entries()).unwrap();
        assert!(!serialized.contains("raw-tool-secret"));
        assert!(
            !serde_json::to_string(&events.events())
                .unwrap()
                .contains("raw-tool-secret")
        );
        assert!(
            matches!(&provider.requests()[1].messages[2], Message::ToolResult(output) if output.is_error && output.content.starts_with("Withheld: "))
        );
    }
}

struct Observer {
    seen: Arc<Mutex<Vec<&'static str>>>,
}
#[async_trait]
impl Hook for Observer {
    async fn on_event(&self, event: &Event, _ctx: &HookContext) -> HookResult<()> {
        assert!(
            !matches!(event.event, EventKind::HookFailed { .. }),
            "diagnostics must not recurse"
        );
        self.seen.lock().unwrap().push(event.event.kind());
        Err("observer unavailable".into())
    }

    fn observes(&self, _event: &EventKind) -> bool {
        true
    }
}

#[tokio::test]
async fn observer_failures_are_diagnostic_and_retry_events_are_observed_once() {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::fail(ProviderErrorKind::Unavailable, "HTTP 503: retry"),
        FauxStep::text("done"),
    ]));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut config = config(provider, vec![]);
    config.hooks.push("observer", Arc::new(Observer { seen: seen.clone() }));
    let (runner, events) = RunnerSetup {
        retry: fast_retry(1),
        ..RunnerSetup::new(config)
    }
    .build()
    .await;
    let report = runner.run("go").await;
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
impl Hook for CustomContinue {
    async fn finish_step(&self, step: &FinishedStep<'_>, _ctx: &HookContext) -> HookResult<StepDecision> {
        Ok(if step.response.stop_reason == StopReason::Length {
            StepDecision::ContinueAnswer("接着回答".into())
        } else {
            StepDecision::Default
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
    config.hooks.push("custom_continue", Arc::new(CustomContinue));
    let (runner, _) = RunnerSetup::new(config).build().await;
    let report = runner.run("go").await;
    assert_eq!(report.final_text, "第一部分，第二部分。");
    assert!(matches!(&provider.requests()[1].messages[2], Message::User { content, .. } if content == "接着回答"));
}

#[tokio::test]
async fn continuation_budget_is_not_reset_after_a_provider_retry() {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::truncated("甲"),
        FauxStep::fail(ProviderErrorKind::Unavailable, "HTTP 503"),
        FauxStep::truncated("乙"),
        FauxStep::text("never"),
    ]));
    let mut config = config(provider.clone(), vec![]);
    config
        .hooks
        .push("continue_on_length", Arc::new(ContinueOnLength::new(1)));
    let (runner, _) = RunnerSetup {
        retry: fast_retry(1),
        limits: RunLimits::default(),
        ..RunnerSetup::new(config)
    }
    .build()
    .await;
    let report = runner.run("go").await;
    assert_eq!(
        (report.outcome, report.model_requests, report.retries),
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
            &zx_testing::hook_context(),
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
            &zx_testing::hook_context(),
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
            &zx_testing::hook_context(),
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
            &zx_testing::hook_context(),
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
            &ToolContext::new(dir.path().to_path_buf(), CancellationToken::new()),
        )
        .await
        .unwrap();
    assert!(printed.content.contains("p\u{FFFD}ssword"), "{}", printed.content);
    let hook = RedactSecrets::new(Vec::new()).with_secret_files([dir.path().join("password")]);

    let output = hook
        .after_tool(&redaction_call(), printed, &zx_testing::hook_context())
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
        .after_tool(&redaction_call(), ToolOutput::ok("output"), &zx_testing::hook_context())
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

#[test]
fn secret_files_with_short_values_are_found() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("username"), "admin\n").unwrap();
    std::fs::write(dir.path().join("port"), "5432").unwrap();
    std::fs::write(dir.path().join("token"), "token-value").unwrap();
    // Blank values redact nothing, so they are not short values.
    std::fs::write(dir.path().join("empty"), " \n").unwrap();
    // Eight characters, counted as characters rather than bytes.
    std::fs::write(dir.path().join("eight"), "界界界界界界界界").unwrap();

    let short = short_secret_files(&[dir.path().to_path_buf(), dir.path().join("absent")]);

    assert_eq!(short, [dir.path().join("port"), dir.path().join("username")]);
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
            &zx_testing::hook_context(),
        )
        .await
        .unwrap();
    assert_eq!(output.content, "[REDACTED] token-hidden token-x");
    // A key added to a Secret volume later shows up as a new file in the same directory.
    std::fs::write(dir.path().join("cluster-b"), "token-b").unwrap();
    let output = hook
        .after_tool(
            &redaction_call(),
            ToolOutput::ok("token-b"),
            &zx_testing::hook_context(),
        )
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
            &zx_testing::hook_context(),
        )
        .await
        .unwrap();
    assert_eq!(output.content, vec!["[REDACTED]"; count].join(" "));
}

struct NextTurn(StepDecision);
#[async_trait]
impl Hook for NextTurn {
    async fn finish_step(&self, _step: &FinishedStep<'_>, _ctx: &HookContext) -> HookResult<StepDecision> {
        Ok(self.0.clone())
    }
}

#[tokio::test]
async fn a_hook_follow_up_obeys_the_step_budget() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("first"), FauxStep::text("never")]));
    let mut config = config(provider.clone(), vec![]);
    config
        .hooks
        .push("next_turn", Arc::new(NextTurn(StepDecision::FollowUp("next".into()))));
    let (runner, _) = RunnerSetup {
        limits: RunLimits {
            max_model_requests: Some(1),
            ..RunLimits::default()
        },
        ..RunnerSetup::new(config)
    }
    .build()
    .await;
    let report = runner.run("go").await;
    assert_eq!(
        report.outcome,
        RunOutcome::BudgetExhausted {
            budget: Budget::ModelRequests,
            truncated: false
        }
    );
    assert_eq!(provider.requests().len(), 1);
}

struct StopAndQueue;
#[async_trait]
impl Hook for StopAndQueue {
    async fn on_event(&self, event: &Event, ctx: &HookContext) -> HookResult<()> {
        if matches!(event.event, EventKind::StepEnd { .. }) {
            ctx.queues.steer("pending user input");
        }
        Ok(())
    }
    async fn finish_step(&self, _step: &FinishedStep<'_>, _ctx: &HookContext) -> HookResult<StepDecision> {
        Ok(StepDecision::Stop)
    }
}

#[tokio::test]
async fn stopping_from_a_hook_preserves_queued_user_input() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("done")]));
    let mut config = config(provider, vec![]);
    config.hooks.push("stop_and_queue", Arc::new(StopAndQueue));
    let (runner, _) = RunnerSetup::new(config).build().await;
    let result = runner.run("go").await;
    assert_eq!(result.outcome, RunOutcome::Completed);
    assert_eq!(
        take_queued_inputs(&runner).unwrap(),
        [Input::from("pending user input")]
    );
}

struct FinishFailure;
#[async_trait]
impl Hook for FinishFailure {
    async fn finish_step(&self, _step: &FinishedStep<'_>, _ctx: &HookContext) -> HookResult<StepDecision> {
        Err("HTTP 503 from extension".into())
    }
}

#[tokio::test]
async fn a_finish_hook_error_never_triggers_provider_retry() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("answer"), FauxStep::text("never")]));
    let mut config = config(provider.clone(), vec![]);
    config.hooks.push("finish_failure", Arc::new(FinishFailure));
    let (runner, _) = RunnerSetup {
        retry: fast_retry(3),
        ..RunnerSetup::new(config)
    }
    .build()
    .await;
    let report = runner.run("go").await;
    assert_eq!(
        (report.outcome, report.model_requests, report.retries),
        (RunOutcome::Failed, 1, 0)
    );
    assert!(report.error.unwrap().contains("HTTP 503 from extension"));
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn an_attempt_that_a_hook_failure_stops_reports_an_error() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("answer")]));
    let mut config = config(provider, vec![]);
    config.hooks.push("finish_failure", Arc::new(FinishFailure));

    let (_, events) = run_prompt(config, &mut Vec::new(), "go").await;

    let ended: Vec<RunOutcome> = events
        .events()
        .iter()
        .filter_map(|event| match event {
            EventKind::AttemptEnd { outcome } => Some(*outcome),
            _ => None,
        })
        .collect();
    assert_eq!(ended, [RunOutcome::Failed]);
}

/// Holds input that starts with "hold" until the run is cancelled or dropped, unless released,
/// and reports when it starts holding.
struct HoldInput {
    holding: Arc<tokio::sync::Notify>,
    released: Arc<AtomicBool>,
}
#[async_trait]
impl Hook for HoldInput {
    async fn on_input(&self, text: String, _ctx: &HookContext) -> HookResult<String> {
        if text.starts_with("hold") && !self.released.load(Ordering::SeqCst) {
            self.holding.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(text)
    }
}

async fn holding_runner(provider: Arc<FauxProvider>) -> (zx_testing::Driven, Arc<zx_testing::Collect>, HoldInput) {
    let holding = Arc::new(tokio::sync::Notify::new());
    let released = Arc::new(AtomicBool::new(false));
    let mut config = config(provider, vec![]);
    config.hooks.push(
        "hold_input",
        Arc::new(HoldInput {
            holding: holding.clone(),
            released: released.clone(),
        }),
    );
    let (runner, events) = RunnerSetup::new(config).build().await;
    (runner, events, HoldInput { holding, released })
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
    let (runner, _, hold) = holding_runner(provider.clone()).await;

    let (report, ()) = tokio::join!(runner.run("hold: rotate the logs"), async {
        hold.holding.notified().await;
        runner.cancel();
    });

    assert_eq!(report.outcome, RunOutcome::Cancelled);
    assert_eq!(mentions(&runner.session().messages(), "rotate the logs"), 0);
    let report = runner.run("next").await;
    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(summarize(&provider.requests()[0].messages), ["user:next"]);
}

#[tokio::test]
async fn a_hook_that_the_cancellation_stopped_is_not_reported_as_failed() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("done")]));
    let (runner, events, hold) = holding_runner(provider).await;

    let (report, ()) = tokio::join!(runner.run("hold: rotate the logs"), async {
        hold.holding.notified().await;
        runner.cancel();
    });

    assert_eq!(report.outcome, RunOutcome::Cancelled);
    let failed: Vec<EventKind> = events
        .events()
        .into_iter()
        .filter(|event| matches!(event, EventKind::HookFailed { .. }))
        .collect();
    assert!(failed.is_empty(), "{failed:?}");
}

#[tokio::test]
async fn queued_input_whose_run_was_dropped_stays_queued_and_unrecorded() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("done")]));
    let (runner, _, hold) = holding_runner(provider.clone()).await;
    runner.queues().steer("hold: check the disk");

    tokio::select! {
        _ = runner.run("go") => panic!("the input hook should still hold the input"),
        () = hold.holding.notified() => {}
    }

    assert_eq!(mentions(&runner.session().messages(), "check the disk"), 0);
    hold.released.store(true, Ordering::SeqCst);
    let report = runner.run("next").await;
    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(
        summarize(&provider.requests()[0].messages),
        ["user:go", "user:next", "user:hold: check the disk"]
    );
}

/// The policy verdicts and approvals of a run, in order, as `(call, what)` pairs.
fn approvals(events: &zx_testing::Collect) -> Vec<(String, String)> {
    events
        .events()
        .into_iter()
        .filter_map(|event| match event {
            EventKind::ToolDecision {
                tool_call_id, decision, ..
            } => Some((tool_call_id, format!("{decision:?}"))),
            EventKind::ApprovalRequested { tool_call_id, .. } => Some((tool_call_id, "requested".into())),
            EventKind::ApprovalResolved {
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
    for verdict in [PolicyDecision::Allow, PolicyDecision::Deny("not on the list".into())] {
        let provider = Arc::new(FauxProvider::new([
            FauxStep::tool_call("c1", "echo", json!({})),
            FauxStep::text("done"),
        ]));
        let mut config = config(provider, vec![Arc::new(EchoTool::new("echo"))]);
        config.policies.push("verdict", Arc::new(Verdict(verdict.clone())));

        let (_, events) = run_prompt(config, &mut Vec::new(), "go").await;

        let decisions: Vec<_> = events
            .events()
            .into_iter()
            .filter_map(|event| match event {
                EventKind::ToolDecision { decision, .. } => Some(decision),
                _ => None,
            })
            .collect();
        assert_eq!(decisions, [verdict]);
        assert_eq!(approvals(&events).len(), 1, "nobody is asked");
    }
}

#[tokio::test]
async fn approvals_are_reported_when_asked_and_when_remembered() {
    for (approval, expected) in [
        (
            ApprovalDecision::AllowOnce,
            pairs(&[
                ("c1", r#"Ask("review")"#),
                ("c1", "requested"),
                ("c1", "AllowOnce"),
                ("c2", r#"Ask("review")"#),
                ("c2", "requested"),
                ("c2", "AllowOnce"),
            ]),
        ),
        (
            ApprovalDecision::AllowSession,
            pairs(&[
                ("c1", r#"Ask("review")"#),
                ("c1", "requested"),
                ("c1", "AllowSession"),
                ("c2", r#"Ask("review")"#),
                ("c2", "Remembered"),
            ]),
        ),
        (
            ApprovalDecision::Deny("declined".into()),
            pairs(&[
                ("c1", r#"Ask("review")"#),
                ("c1", "requested"),
                ("c1", "Denied"),
                ("c2", r#"Ask("review")"#),
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
        config
            .policies
            .push("verdict", Arc::new(Verdict(PolicyDecision::Ask("review".into()))));
        let host = Arc::new(Approver {
            approval,
            calls: AtomicUsize::new(0),
        });

        let (_, events) = run_approved(config, host, "go").await;

        assert_eq!(approvals(&events), expected);
    }
}

/// Answers after `delay`, like a person who takes a moment.
struct SlowApprover(Duration);
#[async_trait]
impl zx_testing::Approver for SlowApprover {
    async fn confirm(&self, _request: &Asked) -> Option<ApprovalDecision> {
        tokio::time::sleep(self.0).await;
        Some(ApprovalDecision::AllowOnce)
    }
}

#[tokio::test]
async fn a_resolved_approval_reports_how_long_it_waited() {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c1", "echo", json!({})),
        FauxStep::text("done"),
    ]));
    let mut config = config(provider, vec![Arc::new(EchoTool::new("echo"))]);
    config
        .policies
        .push("verdict", Arc::new(Verdict(PolicyDecision::Ask("review".into()))));
    let host = Arc::new(SlowApprover(Duration::from_millis(20)));

    let (_, events) = run_approved(config, host, "go").await;

    let waited: Vec<u64> = events
        .events()
        .into_iter()
        .filter_map(|event| match event {
            EventKind::ApprovalResolved { wait_ms, .. } => Some(wait_ms),
            _ => None,
        })
        .collect();
    assert_eq!(waited.len(), 1);
    assert!(waited[0] >= 20, "{waited:?}");
}

#[tokio::test]
async fn an_approval_ends_as_denied_without_anyone_to_ask_and_as_timed_out_without_an_answer() {
    for (approvals, ended) in [
        (ApprovalMode::Deny, ApprovalResult::Denied),
        (ApprovalMode::Wait(Duration::ZERO), ApprovalResult::TimedOut),
    ] {
        let provider = Arc::new(FauxProvider::new([
            FauxStep::tool_call("c", "echo", json!({})),
            FauxStep::text("handled"),
        ]));
        let mut config = config(provider, vec![Arc::new(EchoTool::new("echo"))]);
        config
            .policies
            .push("verdict", Arc::new(Verdict(PolicyDecision::Ask("review".into()))));
        config.approvals = approvals;

        let (_, events) = run_prompt(config, &mut Vec::new(), "go").await;

        let resolved: Vec<_> = events
            .events()
            .into_iter()
            .filter_map(|event| match event {
                EventKind::ApprovalResolved {
                    approval_id,
                    approval,
                    reason,
                    ..
                } => Some((approval, approval_id.is_some(), reason.is_some())),
                _ => None,
            })
            .collect();
        assert_eq!(resolved, [(ended, true, true)]);
    }
}

#[tokio::test]
async fn an_approval_interrupted_by_cancellation_is_reported_as_cancelled() {
    let provider = Arc::new(FauxProvider::new([FauxStep::tool_call("c", "echo", json!({}))]));
    let entered = Arc::new(tokio::sync::Notify::new());
    let mut config = config(provider, vec![Arc::new(EchoTool::new("echo"))]);
    config
        .policies
        .push("verdict", Arc::new(Verdict(PolicyDecision::Ask("review".into()))));
    let (runner, events) = RunnerSetup {
        approver: Some(Arc::new(WaitingHost(entered.clone()))),
        ..RunnerSetup::new(config)
    }
    .build()
    .await;

    let (report, ()) = tokio::join!(runner.run("go"), async {
        entered.notified().await;
        runner.cancel();
    });

    assert_eq!(report.outcome, RunOutcome::Cancelled);
    assert_eq!(
        approvals(&events),
        pairs(&[("c", r#"Ask("review")"#), ("c", "requested"), ("c", "Cancelled")])
    );
}

/// Records the kinds of the events it observes; `all` also observes streaming deltas.
struct Kinds {
    seen: Arc<Mutex<Vec<&'static str>>>,
    all: bool,
}
#[async_trait]
impl Hook for Kinds {
    async fn on_event(&self, event: &Event, _ctx: &HookContext) -> HookResult<()> {
        self.seen.lock().unwrap().push(event.event.kind());
        Ok(())
    }

    fn observes(&self, event: &EventKind) -> bool {
        self.all || !matches!(event, EventKind::MessageUpdate { .. })
    }
}

#[tokio::test]
async fn observers_skip_streaming_deltas_unless_they_ask_for_them() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("done")]));
    let mut config = config(provider, vec![]);
    let default = Arc::new(Mutex::new(Vec::new()));
    let all = Arc::new(Mutex::new(Vec::new()));
    config.hooks.push(
        "kinds",
        Arc::new(Kinds {
            seen: default.clone(),
            all: false,
        }),
    );
    config.hooks.push(
        "kinds",
        Arc::new(Kinds {
            seen: all.clone(),
            all: true,
        }),
    );

    let (_, events) = run_prompt(config, &mut Vec::new(), "go").await;

    let kinds = events.kinds();
    assert!(kinds.contains(&"message_update"), "the provider streamed deltas");
    assert_eq!(*all.lock().unwrap(), kinds);
    let without_deltas: Vec<_> = kinds.into_iter().filter(|kind| *kind != "message_update").collect();
    assert_eq!(*default.lock().unwrap(), without_deltas);
}

/// What each hook method of a [`ContextProbe`] received: method, session, run and directory.
type Seen = Arc<Mutex<Vec<(&'static str, String, String, PathBuf)>>>;

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
impl Hook for ContextProbe {
    async fn on_input(&self, text: String, ctx: &HookContext) -> HookResult<String> {
        self.record("on_input", ctx);
        Ok(text)
    }
    async fn transform_context(&self, _messages: &[Message], ctx: &HookContext) -> HookResult<Vec<ContextEdit>> {
        self.record("transform_context", ctx);
        Ok(Vec::new())
    }
    async fn after_tool(&self, _call: &ToolCall, output: ToolOutput, ctx: &HookContext) -> HookResult<ToolOutput> {
        self.record("after_tool", ctx);
        Ok(output)
    }
    async fn finish_step(&self, _step: &FinishedStep<'_>, ctx: &HookContext) -> HookResult<StepDecision> {
        self.record("finish_step", ctx);
        Ok(StepDecision::Default)
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
    config.hooks.push("probe", Arc::new(probe.clone()));
    let (runner, events) = RunnerSetup {
        session: Start::New(SessionOptions::new("/srv/work")),
        ..RunnerSetup::new(config)
    }
    .build()
    .await;

    runner.run("go").await;

    let session_id = runner.session().header().id.clone();
    let run_id = events.envelopes()[0].run_id.clone();
    let seen = probe.seen.lock().unwrap().clone();
    let mut methods: Vec<&str> = seen.iter().map(|(method, ..)| *method).collect();
    methods.sort_unstable();
    methods.dedup();
    assert_eq!(
        methods,
        ["after_tool", "finish_step", "on_event", "on_input", "transform_context"]
    );
    for (method, session, run, cwd) in &seen {
        assert_eq!(session, &session_id, "{method}");
        assert_eq!(run, &run_id, "{method}: the run ID of the events");
        assert_eq!(cwd, Path::new("/srv/work"), "{method}");
    }
    let cancel = probe.cancel.lock().unwrap().clone().unwrap();
    assert!(cancel.is_cancelled(), "the token is the run's, which ends with the run");
}

struct FailingObserver;
#[async_trait]
impl Hook for FailingObserver {
    async fn on_event(&self, _event: &Event, _ctx: &HookContext) -> HookResult<()> {
        Err("exporter offline".into())
    }
}

#[tokio::test]
async fn a_named_hook_is_reported_under_its_name() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("done")]));
    let mut config = config(provider, vec![]);
    config.hooks.push("trace-exporter", Arc::new(FailingObserver));
    let (runner, events) = RunnerSetup::new(config).build().await;

    runner.run("go").await;

    let hooks: Vec<String> = events
        .events()
        .into_iter()
        .filter_map(|event| match event {
            EventKind::HookFailed { hook, .. } => Some(hook),
            _ => None,
        })
        .collect();
    assert!(!hooks.is_empty());
    assert!(hooks.iter().all(|hook| hook == "trace-exporter"), "{hooks:?}");
}

/// Records its tag in after_tool; runs in the stage it declares.
struct Staged {
    stage: HookStage,
    tag: &'static str,
    order: Arc<Mutex<Vec<String>>>,
}
#[async_trait]
impl Hook for Staged {
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
        (HookStage::Default, "default-1"),
        (HookStage::Default, "default-2"),
    ] {
        config.hooks.push(
            "staged",
            Arc::new(Staged {
                stage,
                tag,
                order: order.clone(),
            }),
        );
    }

    run_prompt(config, &mut Vec::new(), "go").await;

    let order = order.lock().unwrap().clone();
    let expected: Vec<String> = ["default-1", "default-2", "redaction", "storage"]
        .iter()
        .map(|tag| format!("after:{tag}"))
        .collect();
    assert_eq!(order, expected);
}

/// An extension that copies a secret into tool output.
struct LeakSecret;
#[async_trait]
impl Hook for LeakSecret {
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
    config.hooks.push(
        "redact_secrets",
        Arc::new(RedactSecrets::new(Some("sk-registered-early".into()))),
    );
    config.hooks.push("leak_secret", Arc::new(LeakSecret));

    run_prompt(config, &mut Vec::new(), "go").await;

    let sent = summarize(&provider.requests()[1].messages);
    assert_eq!(sent.last().unwrap(), "tool:c1=ok [REDACTED]");
}

#[tokio::test]
async fn progress_passes_only_the_redaction_hooks() {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call(
            "c1",
            "echo",
            json!({"text": "ok", "progress": ["plain", "with sk-registered-early inside"]}),
        ),
        FauxStep::text("done"),
    ]));
    let mut config = config(provider, vec![Arc::new(EchoTool::new("echo"))]);
    config.hooks.push(
        "redact_secrets",
        Arc::new(RedactSecrets::new(Some("sk-registered-early".into()))),
    );
    // An extension that changes final output must not see transient progress.
    config.hooks.push("leak_secret", Arc::new(LeakSecret));

    let (_, sink) = run_prompt(config, &mut Vec::new(), "go").await;

    let updates: Vec<String> = sink
        .events()
        .into_iter()
        .filter_map(|event| match event {
            EventKind::ToolExecutionUpdate { content, .. } => Some(content),
            _ => None,
        })
        .collect();
    assert_eq!(updates, ["plain", "with [REDACTED] inside"]);
}

#[tokio::test]
async fn redaction_covers_the_strings_and_keys_of_details() {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call(
            "c1",
            "echo",
            json!({"text": "ok", "details": {"sk-registered-early": ["token sk-registered-early", 7, null]}}),
        ),
        FauxStep::text("done"),
    ]));
    let mut config = config(provider, vec![Arc::new(EchoTool::new("echo"))]);
    config.hooks.push(
        "redact_secrets",
        Arc::new(RedactSecrets::new(Some("sk-registered-early".into()))),
    );

    let (_, sink) = run_prompt(config, &mut Vec::new(), "go").await;

    let details: Vec<_> = sink
        .events()
        .into_iter()
        .filter_map(|event| match event {
            EventKind::ToolExecutionEnd { details, .. } => details,
            _ => None,
        })
        .collect();
    assert_eq!(details, [json!({"[REDACTED]": ["token [REDACTED]", 7, null]})]);
}

#[test]
fn tools_with_the_same_name_refuse_the_agent() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("never sent")]));
    let config = config(
        provider,
        vec![Arc::new(EchoTool::new("echo")), Arc::new(EchoTool::new("echo"))],
    );

    assert_eq!(refusal(config), "more than one tool is named \"echo\"");
}

struct AppendSystem;
#[async_trait]
impl Hook for AppendSystem {
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
    config.hooks.push("append_system", Arc::new(AppendSystem));

    let (result, _) = run_prompt(config, &mut Vec::new(), "go").await;

    assert_eq!(result.outcome, RunOutcome::Failed);
    let error = format!("{:?}", result.error);
    assert!(error.contains("system prompt comes from the configuration"), "{error}");
    assert!(provider.requests().is_empty());
}

struct PanickingObserver;
#[async_trait]
impl Hook for PanickingObserver {
    fn observes(&self, _event: &EventKind) -> bool {
        panic!("observes is broken")
    }
}

#[tokio::test]
async fn a_panic_in_observes_is_reported_and_the_run_continues() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("done")]));
    let mut config = config(provider, vec![]);
    config.hooks.push("panicking_observer", Arc::new(PanickingObserver));

    let (result, sink) = run_prompt(config, &mut Vec::new(), "go").await;

    assert_eq!(result.outcome, RunOutcome::Completed);
    assert!(sink.kinds().contains(&"hook_failed"), "{:?}", sink.kinds());
}

/// A tool that may declare itself under another name, or panic while declaring itself.
struct Declared {
    name: &'static str,
    declared: &'static str,
    panics: bool,
}

#[async_trait]
impl zx_agent::tools::Tool for Declared {
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
        _ctx: &zx_agent::tools::ToolContext,
    ) -> Result<ToolOutput, String> {
        Ok(ToolOutput::ok("ran"))
    }
}

#[test]
fn a_tool_declared_under_another_name_refuses_the_agent() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("never sent")]));
    let tool = Declared {
        name: "echo",
        declared: "other",
        panics: false,
    };

    let error = refusal(config(provider, vec![Arc::new(tool)]));

    assert!(error.contains("declares itself as \"other\""), "{error}");
}

#[test]
fn a_tool_whose_definition_panics_refuses_the_agent() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("never sent")]));
    let tool = Declared {
        name: "echo",
        declared: "echo",
        panics: true,
    };

    let error = refusal(config(provider, vec![Arc::new(tool)]));

    assert_eq!(error, "tool 0 panicked while describing itself");
}

/// Cancels the token it is given, as a tool might to stop work of its own.
struct CancelsItself;

#[async_trait]
impl zx_agent::tools::Tool for CancelsItself {
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
        ctx: &zx_agent::tools::ToolContext,
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

    let (result, _) = run_prompt(config, &mut context, "go").await;

    assert_eq!(result.outcome, RunOutcome::Completed);
    assert!(
        summarize(&context).contains(&"tool:c1=stopped its own work".to_string()),
        "{:?}",
        summarize(&context)
    );
}
