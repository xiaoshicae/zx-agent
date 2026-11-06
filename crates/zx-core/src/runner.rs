//! Run-level control around the loop (a small subset of pi's coding-agent AgentSession).
//!
//! The loop stops at the first provider error; this layer decides whether to retry, keeps
//! the session as the source of model context, and enforces request, time and token limits.
//!
//! - `run_host`: what a run lends the loop and the summarizer, and how a run opens and closes
//! - `interrupted`: results for the tool calls an interrupted run left unfinished
//! - `accounting`: the costs of a run's attempts, and the budget that remains
//! - `final_answer`: the answer, joined from the parts that continue a cut-off one

mod accounting;
mod final_answer;
mod interrupted;
mod run_host;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use accounting::RunAccounting;
use run_host::{CompactionPause, RunHost, RunStop};

use crate::agent::{
    EventSink, EventTarget, LoopConfig, LoopControl, LoopFailure, LoopOutcome, QueuedMessage, RunTrace, run_loop,
};
pub use crate::agent::{RunOutcome, RunReport};
use crate::ai::retry::{RetryPolicy, is_context_overflow, is_retryable};
use crate::ai::{AssistantMessage, Provider, Reasoning};
use crate::compaction::{self, Trigger};
use crate::interaction::ApprovalCache;
use crate::session::SessionStore;
use crate::sync::AbortOnDrop;

/// Bounds for one run. Defaults impose no request or time limit.
/// Continuation is supplied by a finish_turn hook.
#[derive(Clone, Debug, Default)]
pub struct Limits {
    /// Model requests per run, failed attempts included. `None` disables the limit;
    /// `Some(0)` prevents any model request.
    pub max_model_requests: Option<u32>,
    /// Wall-clock limit for the whole run; `None` disables it.
    pub timeout: Option<Duration>,
    /// Tokens the run may use: input plus output of all its model requests as the provider
    /// reports them, summary requests and failed attempts included. Checked before each
    /// request, so the last one may go over. `None` disables the budget.
    pub token_budget: Option<u64>,
}

/// Runs prompts against one session, adding retries, limits, and persistence to the loop.
pub struct Runner {
    config: LoopConfig,
    retry: RetryPolicy,
    limits: Limits,
    session: SessionStore,
    sink: Arc<dyn EventSink>,
    approvals: ApprovalCache,
    compaction: compaction::Config,
    pause: CompactionPause,
}

impl Runner {
    /// Create a runner. Each step of a run is recorded in `session` before it takes effect;
    /// events go to `sink` afterwards.
    pub fn new(
        config: LoopConfig,
        retry: RetryPolicy,
        limits: Limits,
        session: SessionStore,
        sink: Arc<dyn EventSink>,
    ) -> Self {
        Self {
            config,
            retry,
            limits,
            session,
            sink,
            approvals: ApprovalCache::default(),
            compaction: compaction::Config::default(),
            pause: CompactionPause::default(),
        }
    }

    /// Opt into automatic context preparation or tune manual summary budgets.
    /// Rejects invalid budgets without making requests or changing the session.
    pub fn with_compaction(mut self, config: compaction::Config) -> anyhow::Result<Self> {
        config.validate()?;
        self.compaction = config;
        Ok(self)
    }

    /// Summarize the old prefix without executing a task or tools. Uses the same exclusive
    /// Runner borrow as run; cancellation and future drop cancel only this operation. Its
    /// events carry a `runId` of their own.
    pub async fn compact(&mut self, cancel: &CancellationToken) -> compaction::Report {
        let child = cancel.child_token();
        let _cancel_on_drop = child.drop_guard_ref();
        let trace = self.config.trace(Some(self.session.header().id.clone()), child.clone());
        let target = EventTarget {
            sink: self.sink.as_ref(),
            trace: &trace,
        };
        let mut host = RunHost::new(
            &mut self.session,
            target,
            &self.config,
            &self.compaction,
            &mut self.pause,
        );
        host.summarize(Trigger::Manual, &child).await
    }

    /// The loop configuration, for example to reach `config().queues` during a run.
    pub fn config(&self) -> &LoopConfig {
        &self.config
    }

    pub(crate) fn replace_sink(&mut self, sink: Arc<dyn EventSink>) -> Arc<dyn EventSink> {
        std::mem::replace(&mut self.sink, sink)
    }

    pub(crate) fn replace_model(&mut self, provider: Arc<dyn Provider>, model: String) -> (Arc<dyn Provider>, String) {
        // Another model may produce a valid summary where the previous one failed.
        self.pause.clear();
        (
            std::mem::replace(&mut self.config.spec.provider, provider),
            std::mem::replace(&mut self.config.spec.model, model),
        )
    }

    pub(crate) fn replace_reasoning(&mut self, reasoning: Option<Reasoning>) -> Option<Reasoning> {
        std::mem::replace(&mut self.config.spec.reasoning, reasoning)
    }

    /// Revoke exact-call approvals granted by this Runner's interaction host.
    pub fn clear_approvals(&mut self) {
        self.approvals.clear();
    }

    /// The session the runner records in.
    pub fn session(&self) -> &SessionStore {
        &self.session
    }

    /// The session, for what a host does to it between runs: a branch switch, or a record
    /// of its own, such as an extension's state or an idempotency key.
    pub(crate) fn session_mut(&mut self) -> &mut SessionStore {
        &mut self.session
    }

    /// Run one prompt to completion. Never panics on provider or tool failures; the report
    /// says how the run ended.
    ///
    /// One runner owns one active conversation. Overlapping runs are rejected by borrowing:
    ///
    /// ```compile_fail
    /// use tokio_util::sync::CancellationToken;
    /// use zx_core::runner::Runner;
    ///
    /// async fn overlapping(runner: &mut Runner, cancel: &CancellationToken) {
    ///     let first = runner.run("first", cancel);
    ///     let second = runner.run("second", cancel);
    ///     tokio::join!(first, second);
    /// }
    /// ```
    pub async fn run(&mut self, prompt: impl Into<String>, cancel: &CancellationToken) -> RunReport {
        self.run_message(QueuedMessage::User(prompt.into()), cancel).await
    }

    /// Run with any message as the input: the person's text, or a message from an
    /// extension (pi: `sendMessage` with `triggerTurn` while idle). Otherwise like
    /// [`Runner::run`].
    pub async fn run_message(&mut self, message: QueuedMessage, cancel: &CancellationToken) -> RunReport {
        self.start(Start::Message(message), cancel).await
    }

    /// Continue the conversation without new input: the model answers
    /// the session as it is. Runs like [`Runner::run`] otherwise, interrupted calls recovered
    /// first. Whether there is something to answer is checked after that recovery:
    ///
    /// - the conversation ends with input or a tool result: the model answers it, for
    ///   example after a crash left results to recover;
    /// - it ends with an answer that failed or was cancelled, whose calls never ran: that
    ///   answer is omitted with a `context_edit`, as a retry does, and the model answers again;
    /// - it ends with a finished answer: queued steering, or else follow-up, input becomes the
    ///   next input; with none queued the run ends with an error and makes no request, as it
    ///   does for an empty conversation.
    pub async fn continue_run(&mut self, cancel: &CancellationToken) -> RunReport {
        self.start(Start::Continue, cancel).await
    }

    async fn start(&mut self, begin: Start, cancel: &CancellationToken) -> RunReport {
        let run_cancel = cancel.child_token();
        // Dropping this future must cancel background providers even when the caller
        // keeps its parent token alive.
        let _cancel_on_drop = run_cancel.drop_guard_ref();
        let timer = RunTimer::start(self.limits.timeout, &run_cancel);
        let Self {
            config,
            retry,
            limits,
            session,
            sink,
            approvals,
            compaction,
            pause,
        } = self;
        let sink = sink.as_ref();
        let trace = config.trace(Some(session.header().id.clone()), run_cancel.clone());
        let target = EventTarget { sink, trace: &trace };
        let mut host = RunHost::new(session, target, config, compaction, pause);
        let (begun, run_start) = host.open_run(begin).await;
        let mut accounting = RunAccounting::default();
        let mut shared = Attempting {
            config,
            retry,
            limits,
            approvals,
            cancel: &run_cancel,
            sink,
            trace: &trace,
        };
        let stop = match begun {
            Err(error) => RunStop::Failed(error),
            Ok(prompts) => attempts(&mut host, prompts, &mut accounting, &mut shared).await,
        };
        host.close_run(stop, run_start, limits, &accounting, &timer).await
    }
}

/// What every attempt of one run shares.
struct Attempting<'a> {
    config: &'a LoopConfig,
    retry: &'a RetryPolicy,
    limits: &'a Limits,
    approvals: &'a mut ApprovalCache,
    cancel: &'a CancellationToken,
    sink: &'a dyn EventSink,
    trace: &'a RunTrace,
}

/// The reason reported for an attempt that ended with `Error` but named no failure.
const NO_DIAGNOSTIC: &str = "loop failed without a diagnostic";

/// Run `prompts`, then retry or recover from a context overflow as long as the budget and
/// the retry policy allow, until an attempt ends the run.
async fn attempts(
    host: &mut RunHost<'_>,
    mut prompts: Vec<QueuedMessage>,
    accounting: &mut RunAccounting,
    shared: &mut Attempting<'_>,
) -> RunStop {
    loop {
        let attempt = accounting.attempts + 1;
        host.attempt = attempt;
        // The session projection, not an in-memory list, is what the model sees.
        let mut context = host.session.messages();
        let mut control = LoopControl {
            token_budget: accounting.remaining_tokens(shared.limits),
            prior_task_turns: accounting.task_turns,
            prior_answer_continuations: accounting.answer_continuations,
            session: Some(&mut *host),
            trace: Some(shared.trace),
            attempt_number: attempt,
            ..LoopControl::new(
                accounting.remaining_model_requests(shared.limits),
                &mut *shared.approvals,
                shared.cancel,
                shared.sink,
            )
        };
        let result = run_loop(&mut context, std::mem::take(&mut prompts), shared.config, &mut control).await;
        accounting.record(&result);
        if result.outcome != LoopOutcome::Error {
            break RunStop::Ended(result.outcome);
        }
        // Hook failures are distinct from failed provider responses and are not retried.
        let (failed, entry_id) = match result.failure {
            Some(LoopFailure::Provider { message, entry_id }) => (message, entry_id),
            Some(LoopFailure::Host(error)) => break RunStop::Failed(error),
            None => break RunStop::Failed(NO_DIAGNOSTIC.into()),
        };
        let failure = Failure {
            message: failed,
            entry_id,
            task_turns: result.task_turns,
        };
        if let Err(stop) = after_failure(host, &failure, accounting, shared).await {
            break stop;
        }
    }
}

/// A failed provider response and where it happened.
struct Failure {
    message: Box<AssistantMessage>,
    entry_id: Option<String>,
    /// Task turns of the attempt that failed.
    task_turns: u32,
}

/// Recover from a context overflow by compacting once, or schedule a retry; an error says
/// why the run ends instead. Both are requests, so both need budget.
async fn after_failure(
    host: &mut RunHost<'_>,
    failure: &Failure,
    accounting: &mut RunAccounting,
    shared: &Attempting<'_>,
) -> Result<(), RunStop> {
    let failed = &failure.message;
    let error = failed.failure_reason();
    let out_of_budget = !accounting.has_budget(shared.limits) || shared.cancel.is_cancelled();
    if is_context_overflow(failed) && host.compaction.automatic() {
        // A context the model rejects as too long fails again however often it is sent:
        // compact once and continue. Not a retry, so it has its own limit: once per run,
        // until an answer succeeds again (more than one turn in the attempt).
        let refusal = if accounting.overflow_recovered && failure.task_turns <= 1 {
            Some("the context still overflowed after compacting it once".to_string())
        } else {
            host.pause.reason().map(|reason| {
                format!("automatic compaction is paused because it failed earlier in this session ({reason})")
            })
        };
        if let Some(refusal) = refusal {
            return Err(RunStop::Failed(format!("{error}; {refusal}")));
        }
        if out_of_budget {
            return Err(RunStop::Failed(error.to_string()));
        }
        host.recover_overflow(error, failure.entry_id.as_deref(), accounting, shared.cancel)
            .await?;
        accounting.overflow_recovered = true;
        return Ok(());
    }
    // Without budget, the provider error is the reason the run ends.
    if !is_retryable(failed) || accounting.retries >= shared.retry.max_retries || out_of_budget {
        return Err(RunStop::Failed(error.to_string()));
    }
    host.schedule_retry(
        shared.retry,
        accounting,
        failed,
        failure.entry_id.as_deref(),
        shared.cancel,
    )
    .await
}

/// What a run starts from.
enum Start {
    /// The person's text, or a message from an extension.
    Message(QueuedMessage),
    /// The session as it is; see [`Runner::continue_run`].
    Continue,
}

/// Cancels the run when its wall-clock limit passes, and remembers that it did.
struct RunTimer {
    timed_out: Arc<AtomicBool>,
    _task: Option<AbortOnDrop<()>>,
}

impl RunTimer {
    fn start(limit: Option<Duration>, cancel: &CancellationToken) -> Self {
        let timed_out = Arc::new(AtomicBool::new(false));
        let task = limit.map(|limit| {
            let token = cancel.clone();
            let flag = timed_out.clone();
            AbortOnDrop(tokio::spawn(async move {
                tokio::time::sleep(limit).await;
                flag.store(true, Ordering::SeqCst);
                token.cancel();
            }))
        });
        Self { timed_out, _task: task }
    }

    fn timed_out(&self) -> bool {
        self.timed_out.load(Ordering::SeqCst)
    }
}

/// How the run ended, as the report states it.
fn conclude(
    stop: RunStop,
    limits: &Limits,
    accounting: &RunAccounting,
    timed_out: bool,
) -> (RunOutcome, Option<String>) {
    let outcome = match stop {
        RunStop::Failed(error) => return (RunOutcome::Error, Some(error)),
        RunStop::Ended(outcome) => outcome,
    };
    match outcome {
        LoopOutcome::Completed => (RunOutcome::Completed, None),
        LoopOutcome::Truncated => (
            RunOutcome::Truncated,
            Some("the answer was cut off at the output token limit".to_string()),
        ),
        LoopOutcome::Error => (RunOutcome::Error, Some(NO_DIAGNOSTIC.to_string())),
        LoopOutcome::ModelRequestLimit => (
            RunOutcome::ModelRequestLimit,
            Some(format!(
                "model request limit reached (max_model_requests = {})",
                limits
                    .max_model_requests
                    .expect("ModelRequestLimit requires a configured request limit")
            )),
        ),
        LoopOutcome::TokenBudget => (
            RunOutcome::TokenBudget,
            Some(format!(
                "token budget reached ({} tokens used, token_budget = {})",
                accounting.usage.input_and_output(),
                limits
                    .token_budget
                    .expect("TokenBudget requires a configured token budget")
            )),
        ),
        LoopOutcome::Aborted if timed_out => {
            // Only the timer of a configured limit sets `timed_out`.
            let limit = limits.timeout.expect("Timeout requires a configured time limit");
            (
                RunOutcome::Timeout,
                Some(format!("run exceeded the {}s time limit", limit.as_secs())),
            )
        }
        LoopOutcome::Aborted => (RunOutcome::Aborted, Some("run was cancelled".to_string())),
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::agent::{LoopSpec, NullSink, Tool};
    use crate::ai::faux::{FauxProvider, FauxStep};
    use crate::ai::{Message, SystemPrompt};
    use crate::session::{FailingWriter, SessionHeader};

    #[test]
    fn default_turn_budget_is_unbounded() {
        assert_eq!(Limits::default().max_model_requests, None);
    }

    #[test]
    fn default_token_budget_is_unbounded() {
        assert_eq!(Limits::default().token_budget, None);
    }

    /// A loop over `provider` and `tools` with no system prompt.
    fn config(provider: &Arc<FauxProvider>, tools: Vec<Arc<dyn Tool>>) -> LoopConfig {
        LoopConfig::new(LoopSpec {
            provider: provider.clone(),
            model: "m".into(),
            system_prompt: SystemPrompt::default(),
            tools,
            cwd: PathBuf::from("."),
            max_tokens: None,
            reasoning: None,
            sequential_tools: false,
        })
    }

    /// A runner without retries or limits, whose events go nowhere.
    fn runner(config: LoopConfig, session: SessionStore) -> Runner {
        Runner::new(
            config,
            RetryPolicy::default(),
            Limits::default(),
            session,
            Arc::new(NullSink),
        )
    }

    /// Fails only the write of one entry type, like a disk filling up at that record.
    struct RejectEntry(&'static str);
    impl std::io::Write for RejectEntry {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let needle = format!("\"type\":\"{}\"", self.0);
            if bytes.windows(needle.len()).any(|part| part == needle.as_bytes()) {
                Err(std::io::Error::other(format!("disk full on {}", self.0)))
            } else {
                Ok(bytes.len())
            }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn compaction_write_failures_cancel_before_any_further_model_request() {
        for rejected in [
            "format_upgrade",
            "compaction_started",
            "request_config",
            "request_started",
            "compaction",
            "compaction_failed",
        ] {
            let mut header = SessionHeader::new(Path::new("."), "m");
            header.version = 2;
            let mut store = SessionStore::with_writer(header, Box::new(RejectEntry(rejected)));
            let old = store.append_message(Message::user("x".repeat(4096))).await.unwrap();
            store.append_message(Message::user("recent")).await.unwrap();
            let summary = if rejected == "compaction_failed" {
                "invalid JSON".into()
            } else {
                serde_json::json!({"facts":[{"text":"Old observation","sourceEntryIds":[old]}],"inferences":[],"plans":[]}).to_string()
            };
            let provider = Arc::new(FauxProvider::new([
                FauxStep::text(summary),
                FauxStep::text("must not execute"),
            ]));
            let mut runner = runner(config(&provider, vec![]), store)
                .with_compaction(compaction::Config {
                    trigger_bytes: Some(1024),
                    keep_recent_bytes: 1,
                    ..compaction::Config::default()
                })
                .unwrap();
            let report = runner.run("continue", &CancellationToken::new()).await;
            assert_eq!(report.outcome, RunOutcome::Error, "{rejected}: {report:?}");
            assert!(report.error.unwrap().contains(&format!("disk full on {rejected}")));
            assert_eq!(
                provider.requests().len(),
                usize::from(matches!(rejected, "compaction" | "compaction_failed"))
            );
            assert!(runner.session().latest_compaction().is_none());
            assert_eq!(report.retries, 0);
        }
    }

    #[tokio::test]
    async fn a_session_write_failure_cancels_the_run() {
        let provider = Arc::new(FauxProvider::new([FauxStep::text("never sent")]));
        let session = SessionStore::with_writer(SessionHeader::new(Path::new("."), "m"), Box::new(FailingWriter));
        let mut runner = runner(config(&provider, vec![]), session);

        let report = runner.run("go", &CancellationToken::new()).await;

        assert_eq!(report.outcome, RunOutcome::Error);
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
            let session =
                SessionStore::with_writer(SessionHeader::new(Path::new("."), "m"), Box::new(RejectEntry(rejected)));
            let mut runner = runner(config(&provider, vec![]), session);

            let report = runner.run("go", &CancellationToken::new()).await;

            assert_eq!(report.outcome, RunOutcome::Error, "{rejected}");
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

    /// Fails every write that contains `self.0`, like a disk filling up at that record.
    struct RejectText(&'static str);
    impl std::io::Write for RejectText {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.windows(self.0.len()).any(|part| part == self.0.as_bytes()) {
                Err(std::io::Error::other("disk full on a tool result"))
            } else {
                Ok(bytes.len())
            }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
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
            _ctx: &crate::agent::ToolContext,
        ) -> Result<crate::agent::ToolOutput, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(crate::agent::ToolOutput::ok("ran"))
        }
    }

    #[tokio::test]
    async fn a_start_record_write_failure_prevents_the_call() {
        // A v3 session must first record its upgrade to v4; either write can fail.
        for (version, rejected) in [(4, "tool_call_started"), (3, "format_upgrade")] {
            let tool = Arc::new(Counted(std::sync::atomic::AtomicUsize::new(0)));
            let provider = Arc::new(FauxProvider::new([
                FauxStep::tool_call("c1", "counted", serde_json::json!({})),
                FauxStep::text("never sent"),
            ]));
            let mut header = SessionHeader::new(Path::new("."), "m");
            header.version = version;
            let session = SessionStore::with_writer(header, Box::new(RejectEntry(rejected)));
            let mut runner = runner(config(&provider, vec![tool.clone()]), session);

            let report = runner.run("go", &CancellationToken::new()).await;

            assert_eq!(report.outcome, RunOutcome::Error, "{rejected}");
            let error = report.error.unwrap_or_default();
            assert!(
                error.contains(&format!("disk full on {rejected}")),
                "{rejected}: {error}"
            );
            assert_eq!(
                tool.0.load(Ordering::SeqCst),
                0,
                "{rejected}: ran without a start record"
            );
            assert_eq!(provider.requests().len(), 1, "{rejected}");
        }
    }

    #[tokio::test]
    async fn a_recovery_write_failure_cancels_the_run_before_any_request() {
        // Recovery writes tool results; reject them, and only them.
        let mut session = SessionStore::with_writer(
            SessionHeader::new(Path::new("."), "m"),
            Box::new(RejectText("\"role\":\"toolResult\"")),
        );
        session.append_message(Message::user("go")).await.unwrap();
        let mut answer = crate::ai::AssistantMessage::failure("p", "m", crate::ai::StopReason::ToolUse, "");
        answer.error_message = None;
        answer.content = vec![crate::ai::ContentBlock::ToolCall {
            id: "c1".into(),
            name: "counted".into(),
            arguments: serde_json::json!({}),
            arguments_error: None,
        }];
        session.append_message(Message::Assistant(answer)).await.unwrap();
        session.append_tool_call_started("c1", "counted").await.unwrap();
        let tool = Arc::new(Counted(std::sync::atomic::AtomicUsize::new(0)));
        let provider = Arc::new(FauxProvider::new([FauxStep::text("never sent")]));
        let mut runner = runner(config(&provider, vec![tool.clone()]), session);

        let report = runner.run("continue", &CancellationToken::new()).await;

        assert_eq!(report.outcome, RunOutcome::Error);
        let error = report.error.unwrap_or_default();
        assert!(error.contains("disk full on a tool result"), "{error}");
        assert!(provider.requests().is_empty(), "requested without a recorded result");
        assert_eq!(tool.0.load(Ordering::SeqCst), 0, "recovery must not run calls again");
    }

    #[tokio::test]
    async fn a_context_edit_write_failure_prevents_the_request() {
        struct EditFailure;
        impl std::io::Write for EditFailure {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if bytes
                    .windows(b"context_edit".len())
                    .any(|window| window == b"context_edit")
                {
                    Err(std::io::Error::other("disk full on edit"))
                } else {
                    Ok(bytes.len())
                }
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        struct Rewrite;
        #[async_trait::async_trait]
        impl crate::agent::Hooks for Rewrite {
            async fn on_input(
                &self,
                _text: String,
                _ctx: &crate::agent::HookContext,
            ) -> crate::agent::HookResult<String> {
                Ok("rewritten".into())
            }
        }
        let provider = Arc::new(FauxProvider::new([FauxStep::text("never")]));
        let mut config = config(&provider, vec![]);
        config.hooks.push(Rewrite);
        let session = SessionStore::with_writer(SessionHeader::new(Path::new("."), "m"), Box::new(EditFailure));
        let mut runner = runner(config, session);
        let report = runner.run("original", &CancellationToken::new()).await;
        assert_eq!(report.outcome, RunOutcome::Error);
        assert!(report.error.unwrap().contains("disk full on edit"));
        assert!(provider.requests().is_empty());
        assert_eq!(runner.session().messages().len(), 1);
        assert!(matches!(&runner.session().messages()[0], Message::User { content, .. } if content == "original"));
    }
}
