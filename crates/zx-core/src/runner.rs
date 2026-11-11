//! Run-level control around the loop (a small subset of pi's coding-agent AgentSession).
//!
//! The loop stops at the first provider error; this layer decides whether to retry, keeps
//! the session as the source of model context, and enforces request, time and token limits.
//!
//! - `run_host`: what a run lends the loop and the compactor, and how a run opens and closes
//! - `interrupted`: results for the tool calls an interrupted run left unfinished
//! - `final_answer`: the answer, joined from the parts that continue a cut-off one
//!
//! What a run spends is counted in the agent layer's `RunAccounting`, which a run lends
//! the loop of each attempt.

mod final_answer;
mod interrupted;
mod run_host;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use final_answer::final_answer;
use run_host::{CompactionPause, RunHost, RunStop};

use crate::agent::{
    AgentEvent, AgentLoop, ApprovalCache, EventSink, EventTarget, Halt, LoopConfig, QueuedMessage, RunAccounting,
    record_pending_entries,
};
pub use crate::agent::{RunOutcome, RunReport};
use crate::ai::retry::{RetryPolicy, is_context_overflow, is_retryable};
use crate::ai::{AssistantMessage, Provider, Reasoning};
use crate::compaction::{self, Trigger};
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
    /// What the run may spend, in the unit of the configured prices (any currency, as long as
    /// every price uses it): the cost of all its model requests by those prices
    /// ([`LoopConfig::models`](crate::agent::LoopConfig::models)), summary requests and
    /// failed attempts included. Checked before each request, so the last one may go over.
    /// A response from a model without a price costs nothing here, so the host checks that
    /// its models have prices. Tool usage is not counted. `None` disables the budget.
    pub cost_budget: Option<f64>,
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
            hooks: &self.config.hooks,
        };
        let mut host = RunHost {
            session: &mut self.session,
            target,
            config: &self.config,
            compaction: &self.compaction,
            pause: &mut self.pause,
        };
        host.compact(Trigger::Manual, 1).await.into()
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
            std::mem::replace(&mut self.config.provider, provider),
            std::mem::replace(&mut self.config.model, model),
        )
    }

    pub(crate) fn replace_reasoning(&mut self, reasoning: Option<Reasoning>) -> Option<Reasoning> {
        std::mem::replace(&mut self.config.reasoning, reasoning)
    }

    /// The session the runner records in.
    pub fn session(&self) -> &SessionStore {
        &self.session
    }

    /// The session, for what a host does to it between runs: a branch switch, or a record
    /// of its own, such as an extension's state.
    pub(crate) fn session_mut(&mut self) -> &mut SessionStore {
        &mut self.session
    }

    /// Run one prompt to completion: the person's text, or a message from an extension (pi:
    /// `sendMessage` with `triggerTurn` while idle). Never panics on provider or tool
    /// failures; the report says how the run ended.
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
    pub async fn run(&mut self, message: impl Into<QueuedMessage>, cancel: &CancellationToken) -> RunReport {
        self.start(Start::Message(message.into()), cancel).await
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
        let trace = config.trace(Some(session.header().id.clone()), run_cancel.clone());
        let target = EventTarget {
            sink: sink.as_ref(),
            trace: &trace,
            hooks: &config.hooks,
        };
        let run = Run {
            host: RunHost {
                session,
                target,
                config,
                compaction,
                pause,
            },
            retry,
            limits,
            approvals,
            accounting: RunAccounting::new(limits.max_model_requests, limits.token_budget, limits.cost_budget),
            overflow_recovered: false,
            timer,
        };
        run.run(begin).await
    }
}

/// One run in progress: the host it records through, what it borrows from the Runner, and
/// what its attempts have spent so far.
struct Run<'a> {
    host: RunHost<'a>,
    retry: &'a RetryPolicy,
    limits: &'a Limits,
    approvals: &'a mut ApprovalCache,
    accounting: RunAccounting,
    /// A context overflow was recovered from once already.
    overflow_recovered: bool,
    timer: RunTimer,
}

impl Run<'_> {
    /// Open the run from `begin`, run its attempts, and report how it ended.
    async fn run(mut self, begin: Start) -> RunReport {
        // The run's answer is what it adds after the entries the session has now. Counted in
        // entries, whose IDs remain stable even when hooks omit messages from earlier runs.
        let run_start = self.host.session.entries().len();
        let stop = match self.host.open_run(begin).await {
            Ok(prompts) => self.attempts(prompts).await,
            Err(error) => RunStop::Failed(error),
        };
        self.close(stop, run_start).await
    }

    /// Run `prompts`, then retry or recover from a context overflow as long as the budget
    /// and the retry policy allow, until an attempt ends the run.
    async fn attempts(&mut self, mut prompts: Vec<QueuedMessage>) -> RunStop {
        loop {
            let turns_before = self.accounting.task_turns;
            let (failed, entry_id) = match self.attempt(std::mem::take(&mut prompts)).await {
                Halt::Ended(outcome) => break RunStop::Ended(outcome),
                // Hook and recording failures are distinct from failed provider responses and
                // are not retried.
                Halt::Failed(error) => break RunStop::Failed(error),
                Halt::Unrecorded(error) => break RunStop::Failed(format!("{error:#}")),
                Halt::ProviderFailed { message, entry_id } => (message, entry_id),
            };
            let task_turns = self.accounting.task_turns - turns_before;
            if let Err(stop) = self.after_failure(&failed, &entry_id, task_turns).await {
                break stop;
            }
        }
    }

    /// Make one attempt: run the loop, which counts what it spends into the run's
    /// accounting and checks the run's budgets. Returns why the loop stopped.
    async fn attempt(&mut self, prompts: Vec<QueuedMessage>) -> Halt {
        self.accounting.attempts = self.accounting.attempts.saturating_add(1);
        let (config, target) = (self.host.config, self.host.target);
        AgentLoop::new(
            config,
            &mut self.host,
            &mut *self.approvals,
            &mut self.accounting,
            target,
        )
        .run(prompts)
        .await
    }

    /// Recover from a context overflow by compacting once, or schedule a retry; an error
    /// says why the run ends instead. Both are requests, so both need budget. `failed` is
    /// the response that failed, in the session as `entry_id`; `task_turns` counts the turns
    /// of its attempt.
    async fn after_failure(
        &mut self,
        failed: &AssistantMessage,
        entry_id: &str,
        task_turns: u32,
    ) -> Result<(), RunStop> {
        let error = failed.failure_reason();
        let cancelled = self.host.target.trace.cancel().is_cancelled();
        let out_of_budget = self.accounting.exhausted().is_some() || cancelled;
        if is_context_overflow(failed) && self.host.compaction.automatic() {
            // A context the model rejects as too long fails again however often it is sent:
            // compact once and continue. Not a retry, so it has its own limit: once per run,
            // until an answer succeeds again (more than one turn in the attempt).
            let refusal = if self.overflow_recovered && task_turns <= 1 {
                Some("the context still overflowed after compacting it once".to_string())
            } else {
                self.host.pause.reason().map(|reason| {
                    format!("automatic compaction is paused because it failed earlier in this session ({reason})")
                })
            };
            if let Some(refusal) = refusal {
                return Err(RunStop::Failed(format!("{error}; {refusal}")));
            }
            if out_of_budget {
                return Err(RunStop::Failed(error.to_string()));
            }
            self.host
                .recover_overflow(error, entry_id, &mut self.accounting)
                .await?;
            self.overflow_recovered = true;
            return Ok(());
        }
        // Without budget, the provider error is the reason the run ends.
        if !is_retryable(failed) || self.accounting.retries >= self.retry.max_retries || out_of_budget {
            return Err(RunStop::Failed(error.to_string()));
        }
        self.host
            .schedule_retry(self.retry, &mut self.accounting, failed, entry_id)
            .await
    }

    /// Close the run: keep the state hooks asked to record, then report how the run ended
    /// and the answer it added after the first `run_start` entries.
    async fn close(self, stop: RunStop, run_start: usize) -> RunReport {
        // State that hooks asked to record during the last events is kept before the run is
        // reported, so a host reading the session afterwards finds it.
        let (config, target) = (self.host.config, self.host.target);
        let stop = match record_pending_entries(&config.queues, self.host.session, target).await {
            Ok(()) => stop,
            Err(error) => RunStop::Failed(format!("{error:#}")),
        };
        let (outcome, error) = self.conclude(stop);
        let accounting = &self.accounting;
        let session = &self.host.session;
        let report = RunReport {
            outcome,
            final_text: final_answer(&session.messages_since(run_start)),
            model_requests: accounting.model_requests,
            retries: accounting.retries,
            usage: accounting.usage.clone(),
            tool_usage: accounting.tool_usage.clone(),
            denied_calls: accounting.denied_calls,
            error,
            session_file: session.path().map(|path| path.display().to_string()),
        };
        target
            .emit(AgentEvent::RunEnd {
                report: Box::new(report.clone()),
            })
            .await;
        report
    }

    /// How the run ended, as the report states it.
    fn conclude(&self, stop: RunStop) -> (RunOutcome, Option<String>) {
        let (limits, accounting, timed_out) = (self.limits, &self.accounting, self.timer.timed_out());
        let outcome = match stop {
            RunStop::Failed(error) => return (RunOutcome::Error, Some(error)),
            RunStop::Ended(outcome) => outcome,
        };
        match outcome {
            RunOutcome::Completed => (RunOutcome::Completed, None),
            RunOutcome::Truncated => (
                RunOutcome::Truncated,
                Some("the answer was cut off at the output token limit".to_string()),
            ),
            // A failed attempt ends the run as `Failed`, with its reason, never as `Ended`.
            RunOutcome::Error => (RunOutcome::Error, Some("loop failed without a diagnostic".to_string())),
            RunOutcome::ModelRequestLimit => (
                RunOutcome::ModelRequestLimit,
                Some(format!(
                    "model request limit reached (max_model_requests = {})",
                    limits
                        .max_model_requests
                        .expect("ModelRequestLimit requires a configured request limit")
                )),
            ),
            RunOutcome::TokenBudget => (
                RunOutcome::TokenBudget,
                Some(format!(
                    "token budget reached ({} tokens used, token_budget = {})",
                    accounting.usage.input_and_output(),
                    limits
                        .token_budget
                        .expect("TokenBudget requires a configured token budget")
                )),
            ),
            RunOutcome::CostBudget => (
                RunOutcome::CostBudget,
                Some(format!(
                    "cost budget reached ({:.4} spent, cost_budget = {})",
                    accounting.usage.total_cost.unwrap_or(0.0),
                    limits
                        .cost_budget
                        .expect("CostBudget requires a configured cost budget")
                )),
            ),
            // An attempt never ends with `Timeout`: the timer cancels it, and only `timed_out`
            // tells that cancellation from the caller's.
            RunOutcome::Aborted | RunOutcome::Timeout if timed_out => {
                // Only the timer of a configured limit sets `timed_out`.
                let limit = limits.timeout.expect("Timeout requires a configured time limit");
                (
                    RunOutcome::Timeout,
                    Some(format!("run exceeded the {}s time limit", limit.as_secs())),
                )
            }
            RunOutcome::Aborted | RunOutcome::Timeout => (RunOutcome::Aborted, Some("run was cancelled".to_string())),
        }
    }
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

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use std::path::Path;

    use super::*;
    use crate::agent::{NullSink, Tool};
    use crate::ai::Message;
    use crate::ai::faux::{FauxProvider, FauxStep};
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
        LoopConfig {
            tools,
            ..LoopConfig::new(provider.clone(), "m")
        }
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
    async fn compaction_write_failures_cancel_before_any_further_model_request() {
        for rejected in [
            "compaction_started",
            "request_config",
            "request_started",
            "compaction",
            "compaction_failed",
        ] {
            let header = SessionHeader::new(Path::new("."), "m");
            let mut store = SessionStore::with_writer(header, RejectWrites::entry(rejected));
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
    async fn a_cancelled_summary_whose_failure_cannot_be_recorded_ends_the_run_as_an_error() {
        let header = SessionHeader::new(Path::new("."), "m");
        let mut store = SessionStore::with_writer(header, RejectWrites::entry("compaction_failed"));
        store.append_message(Message::user("x".repeat(4096))).await.unwrap();
        store.append_message(Message::user("recent")).await.unwrap();
        let cancel = CancellationToken::new();
        // The run is cancelled while its summary request waits; the session then rejects the
        // record of that failure.
        let cancel_on_request = cancel.clone();
        let provider = Arc::new(FauxProvider::new([FauxStep::compute(move |_| {
            cancel_on_request.cancel();
            FauxStep::Hang
        })]));
        let mut runner = runner(config(&provider, vec![]), store)
            .with_compaction(compaction::Config {
                trigger_bytes: Some(1024),
                keep_recent_bytes: 1,
                ..compaction::Config::default()
            })
            .unwrap();

        let report = runner.run("continue", &cancel).await;

        assert_eq!(report.outcome, RunOutcome::Error, "{report:?}");
        assert!(report.error.unwrap().contains("disk full on compaction_failed"));
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
                SessionStore::with_writer(SessionHeader::new(Path::new("."), "m"), RejectWrites::entry(rejected));
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
        let tool = Arc::new(Counted(std::sync::atomic::AtomicUsize::new(0)));
        let provider = Arc::new(FauxProvider::new([
            FauxStep::tool_call("c1", "counted", serde_json::json!({})),
            FauxStep::text("never sent"),
        ]));
        let session = SessionStore::with_writer(
            SessionHeader::new(Path::new("."), "m"),
            RejectWrites::entry("tool_call_started"),
        );
        let mut runner = runner(config(&provider, vec![tool.clone()]), session);

        let report = runner.run("go", &CancellationToken::new()).await;

        assert_eq!(report.outcome, RunOutcome::Error);
        let error = report.error.unwrap_or_default();
        assert!(error.contains("disk full on tool_call_started"), "{error}");
        assert_eq!(tool.0.load(Ordering::SeqCst), 0, "ran without a start record");
        assert_eq!(provider.requests().len(), 1);
    }

    #[tokio::test]
    async fn a_recovery_write_failure_cancels_the_run_before_any_request() {
        // Recovery writes tool results; reject them, and only them.
        let mut session = SessionStore::with_writer(
            SessionHeader::new(Path::new("."), "m"),
            RejectWrites::containing("\"role\":\"toolResult\"", "a tool result"),
        );
        session.append_message(Message::user("go")).await.unwrap();
        let mut answer = crate::ai::AssistantMessage::failure("p", "m", crate::ai::StopReason::ToolUse, "");
        answer.error_message = None;
        answer.content = vec![crate::ai::ContentBlock::ToolCall(crate::ai::ToolCall {
            id: "c1".into(),
            name: "counted".into(),
            arguments: serde_json::json!({}),
            arguments_error: None,
        })];
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
        let session = SessionStore::with_writer(
            SessionHeader::new(Path::new("."), "m"),
            RejectWrites::containing("context_edit", "edit"),
        );
        let mut runner = runner(config, session);
        let report = runner.run("original", &CancellationToken::new()).await;
        assert_eq!(report.outcome, RunOutcome::Error);
        assert!(report.error.unwrap().contains("disk full on edit"));
        assert!(provider.requests().is_empty());
        assert_eq!(runner.session().messages().len(), 1);
        assert!(matches!(&runner.session().messages()[0], Message::User { content, .. } if content == "original"));
    }
}
