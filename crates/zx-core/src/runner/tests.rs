use std::path::Path;

use async_trait::async_trait;

use super::*;
use crate::ai::faux::{FauxProvider, FauxStep};
use crate::ai::{Message, ProviderErrorKind};
use crate::hooks::{Hook, HookContext, HookResult};
use crate::tools::{Tool, Toolbox};
use crate::transcript::{FailingWriter, SessionHeader};

#[test]
fn default_request_limit_is_unbounded() {
    assert_eq!(RunLimits::default().max_model_requests, None);
}

#[test]
fn default_token_budget_is_unbounded() {
    assert_eq!(RunLimits::default().token_budget, None);
}

/// A loop over `provider` and `tools` with no system prompt.
fn config(provider: &Arc<FauxProvider>, tools: Vec<Arc<dyn Tool>>) -> LoopConfig {
    LoopConfig {
        tools: Toolbox::new(tools).unwrap(),
        ..LoopConfig::new(provider.clone(), "m")
    }
}

/// A runner without retries or limits.
fn runner(config: LoopConfig, transcript: Transcript) -> Runner {
    Runner::new(config, RetryPolicy::default(), RunLimits::default(), transcript)
}

/// Run `prompt` on `runner` to its end, cancelled by `cancel`, with a reader that drops
/// the events.
async fn run(runner: &mut Runner, prompt: &str, cancel: &CancellationToken) -> RunReport {
    let (events, mut reader) = mpsc::channel(1);
    let link = RunLink {
        id: "run".into(),
        cancel: cancel.clone(),
        events,
    };
    let running = runner.run(Start::Message(prompt.into()), &link);
    tokio::pin!(running);
    loop {
        tokio::select! {
            report = &mut running => return report,
            Some(_) = reader.recv() => {}
        }
    }
}

/// Fails every write that contains `needle`, like a disk filling up at that record; the
/// error says "disk full on" `what`.
struct RejectWrites {
    needle: String,
    what: &'static str,
}

impl RejectWrites {
    /// Rejects the records of one entry type, which the error names.
    fn entry(kind: &'static str) -> Box<Self> {
        Self::containing(&format!("\"type\":\"{kind}\""), kind)
    }

    /// Rejects every record that contains `needle`.
    fn containing(needle: &str, what: &'static str) -> Box<Self> {
        Box::new(Self {
            needle: needle.into(),
            what,
        })
    }
}

impl std::io::Write for RejectWrites {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let needle = self.needle.as_bytes();
        if bytes.windows(needle.len()).any(|part| part == needle) {
            Err(std::io::Error::other(format!("disk full on {}", self.what)))
        } else {
            Ok(bytes.len())
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn compaction_write_failures_fail_the_run_before_any_further_model_request() {
    for rejected in [
        "compaction_started",
        "request_config",
        "request_started",
        "compaction",
        "compaction_failed",
    ] {
        let header = SessionHeader::new(Path::new("."), "m");
        let mut transcript = Transcript::with_writer(header, RejectWrites::entry(rejected));
        let old = transcript
            .append_message(Message::user("x".repeat(4096)))
            .await
            .unwrap();
        transcript.append_message(Message::user("recent")).await.unwrap();
        let summary = if rejected == "compaction_failed" {
            "invalid JSON".into()
        } else {
            serde_json::json!({"facts":[{"text":"Old observation","sourceEntryIds":[old]}],"inferences":[],"plans":[]})
                .to_string()
        };
        let provider = Arc::new(FauxProvider::new([
            FauxStep::text(summary),
            FauxStep::text("must not execute"),
        ]));
        let mut runner = runner(config(&provider, vec![]), transcript).with_compaction(compaction::Config {
            trigger_bytes: Some(1024),
            keep_recent_bytes: 1,
            ..compaction::Config::default()
        });
        let report = run(&mut runner, "continue", &CancellationToken::new()).await;
        assert_eq!(report.outcome, RunOutcome::Failed, "{rejected}: {report:?}");
        assert!(report.error.unwrap().contains(&format!("disk full on {rejected}")));
        assert_eq!(
            provider.requests().len(),
            usize::from(matches!(rejected, "compaction" | "compaction_failed"))
        );
        assert!(runner.transcript().latest_compaction().is_none());
        assert_eq!(report.retries, 0);
    }
}

#[tokio::test]
async fn a_cancelled_summary_whose_failure_cannot_be_recorded_ends_the_run_as_an_error() {
    let header = SessionHeader::new(Path::new("."), "m");
    let mut transcript = Transcript::with_writer(header, RejectWrites::entry("compaction_failed"));
    transcript
        .append_message(Message::user("x".repeat(4096)))
        .await
        .unwrap();
    transcript.append_message(Message::user("recent")).await.unwrap();
    let cancel = CancellationToken::new();
    // The run is cancelled while its summary request waits; the session then rejects the
    // record of that failure.
    let cancel_on_request = cancel.clone();
    let provider = Arc::new(FauxProvider::new([FauxStep::compute(move |_| {
        cancel_on_request.cancel();
        FauxStep::Hang
    })]));
    let mut runner = runner(config(&provider, vec![]), transcript).with_compaction(compaction::Config {
        trigger_bytes: Some(1024),
        keep_recent_bytes: 1,
        ..compaction::Config::default()
    });

    let report = run(&mut runner, "continue", &cancel).await;

    assert_eq!(report.outcome, RunOutcome::Failed, "{report:?}");
    assert!(report.error.unwrap().contains("disk full on compaction_failed"));
}

#[tokio::test]
async fn a_session_write_failure_fails_the_run() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("never sent")]));
    let transcript = Transcript::with_writer(SessionHeader::new(Path::new("."), "m"), Box::new(FailingWriter));
    let mut runner = runner(config(&provider, vec![]), transcript);

    let report = run(&mut runner, "go", &CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Failed);
    let error = report.error.unwrap_or_default();
    assert!(error.starts_with("cannot persist session entry"), "{error}");
    assert!(
        provider.requests().is_empty(),
        "no model request without an audit trail"
    );
}

#[tokio::test]
async fn a_request_checkpoint_write_failure_prevents_the_request() {
    for rejected in ["request_config", "request_started"] {
        let provider = Arc::new(FauxProvider::new([FauxStep::text("never sent")]));
        let transcript =
            Transcript::with_writer(SessionHeader::new(Path::new("."), "m"), RejectWrites::entry(rejected));
        let mut runner = runner(config(&provider, vec![]), transcript);

        let report = run(&mut runner, "go", &CancellationToken::new()).await;

        assert_eq!(report.outcome, RunOutcome::Failed, "{rejected}");
        let error = report.error.unwrap_or_default();
        assert!(
            error.contains(&format!("disk full on {rejected}")),
            "{rejected}: {error}"
        );
        assert!(
            provider.requests().is_empty(),
            "{rejected}: dispatched without a checkpoint"
        );
        assert_eq!(report.retries, 0);
    }
}

/// Counts its executions, which must never happen without an audit trail.
struct Counted(std::sync::atomic::AtomicUsize);
#[async_trait]
impl Tool for Counted {
    fn name(&self) -> &str {
        "counted"
    }
    fn definition(&self) -> crate::ai::ToolDef {
        crate::ai::ToolDef {
            name: "counted".into(),
            description: "Counts calls".into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &crate::tools::ToolContext,
    ) -> Result<crate::tools::ToolOutput, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(crate::tools::ToolOutput::ok("ran"))
    }
}

#[tokio::test]
async fn a_start_record_write_failure_prevents_the_call() {
    let tool = Arc::new(Counted(std::sync::atomic::AtomicUsize::new(0)));
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c1", "counted", serde_json::json!({})),
        FauxStep::text("never sent"),
    ]));
    let transcript = Transcript::with_writer(
        SessionHeader::new(Path::new("."), "m"),
        RejectWrites::entry("tool_call_started"),
    );
    let mut runner = runner(config(&provider, vec![tool.clone()]), transcript);

    let report = run(&mut runner, "go", &CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Failed);
    let error = report.error.unwrap_or_default();
    assert!(error.contains("disk full on tool_call_started"), "{error}");
    assert_eq!(tool.0.load(Ordering::SeqCst), 0, "ran without a start record");
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn a_recovery_write_failure_fails_the_run_before_any_request() {
    // Recovery writes tool results; reject them, and only them.
    let mut transcript = Transcript::with_writer(
        SessionHeader::new(Path::new("."), "m"),
        RejectWrites::containing("\"role\":\"toolResult\"", "a tool result"),
    );
    transcript.append_message(Message::user("go")).await.unwrap();
    let mut answer = crate::ai::AssistantMessage::failure("p", "m", crate::ai::StopReason::ToolUse, "");
    answer.error_message = None;
    answer.content = vec![crate::ai::ContentBlock::ToolCall(crate::ai::ToolCall {
        id: "c1".into(),
        name: "counted".into(),
        arguments: serde_json::json!({}),
        arguments_error: None,
    })];
    transcript.append_message(Message::Assistant(answer)).await.unwrap();
    transcript.append_tool_call_started("c1", "counted").await.unwrap();
    let tool = Arc::new(Counted(std::sync::atomic::AtomicUsize::new(0)));
    let provider = Arc::new(FauxProvider::new([FauxStep::text("never sent")]));
    let mut runner = runner(config(&provider, vec![tool.clone()]), transcript);

    let report = run(&mut runner, "continue", &CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Failed);
    let error = report.error.unwrap_or_default();
    assert!(error.contains("disk full on a tool result"), "{error}");
    assert!(provider.requests().is_empty(), "requested without a recorded result");
    assert_eq!(tool.0.load(Ordering::SeqCst), 0, "recovery must not run calls again");
}

#[tokio::test]
async fn a_context_edit_write_failure_prevents_the_request() {
    struct Rewrite;
    #[async_trait]
    impl Hook for Rewrite {
        async fn on_input(&self, _text: String, _ctx: &HookContext) -> HookResult<String> {
            Ok("rewritten".into())
        }
    }
    let provider = Arc::new(FauxProvider::new([FauxStep::text("never")]));
    let mut config = config(&provider, vec![]);
    config.hooks.push("rewrite", Arc::new(Rewrite));
    let transcript = Transcript::with_writer(
        SessionHeader::new(Path::new("."), "m"),
        RejectWrites::containing("context_edit", "edit"),
    );
    let mut runner = runner(config, transcript);
    let report = run(&mut runner, "original", &CancellationToken::new()).await;
    assert_eq!(report.outcome, RunOutcome::Failed);
    assert!(report.error.unwrap().contains("disk full on edit"));
    assert!(provider.requests().is_empty());
    assert_eq!(runner.transcript().messages().len(), 1);
    assert!(matches!(&runner.transcript().messages()[0], Message::User { content, .. } if content == "original"));
}

/// Queues extension state as an attempt ends, after its last step recorded what was queued.
struct NoteAtTheEnd;

#[async_trait]
impl Hook for NoteAtTheEnd {
    async fn on_event(&self, event: &Event, ctx: &HookContext) -> HookResult<()> {
        if matches!(event.event, EventKind::AttemptEnd { .. }) {
            ctx.queues.append_custom_entry("note", serde_json::json!({})).unwrap();
        }
        Ok(())
    }
}

#[tokio::test]
async fn a_failed_run_whose_last_entry_cannot_be_recorded_reports_both_failures() {
    let provider = Arc::new(FauxProvider::new([FauxStep::fail(
        ProviderErrorKind::Other,
        "bad request",
    )]));
    let mut config = config(&provider, vec![]);
    config.hooks.push("note", Arc::new(NoteAtTheEnd));
    let transcript = Transcript::with_writer(SessionHeader::new(Path::new("."), "m"), RejectWrites::entry("custom"));
    let mut runner = runner(config, transcript);

    let report = run(&mut runner, "go", &CancellationToken::new()).await;

    assert_eq!(report.outcome, RunOutcome::Failed);
    let error = report.error.unwrap_or_default();
    assert!(error.contains("bad request"), "{error}");
    assert!(error.contains("disk full on custom"), "{error}");
}
