//! Run-level control around the loop (a small subset of pi's coding-agent AgentSession).
//!
//! The loop stops at the first provider error; this layer decides whether to retry, keeps
//! the session as the source of model context, and enforces request, time and token limits.
//! A [`Session`](crate::session::Session) owns the Runner and lends it to one run at a time.
//!
//! - `step_loop`: the loop a run runs once per attempt, and what it runs with (`LoopConfig`,
//!   which the agent assembles for each session)
//! - `accounting`: what a run spends across its attempts, and its budgets; the loop of each
//!   attempt counts into it
//! - `trace`: one run's identity and event numbering, and where its events go: the run's
//!   reader, then the hooks that observe them
//! - `run_host`: what a run lends the loop and the compactor, how a run opens, and when its
//!   context is summarized
//! - `failure`: what follows a provider failure: a retry, or one recovery from a context
//!   the model rejected as too long
//! - `interrupted`: results for the tool calls an interrupted run left unfinished
//! - `final_answer`: the answer, joined from the parts that continue a cut-off one

mod accounting;
mod failure;
mod final_answer;
mod interrupted;
mod run_host;
mod step_loop;
mod trace;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use final_answer::{cut_off, final_answer};
use run_host::{CompactionPause, RunHost, RunStop};

use crate::ai::retry::RetryPolicy;
use crate::ai::{Provider, Reasoning};
use crate::compaction::{self, Trigger};
use crate::events::{Budget, Event, EventKind, RunOutcome, RunReport};
use crate::input::Input;
use crate::policy::ApprovalGate;
use crate::sync::AbortOnDrop;
use crate::transcript::Transcript;
pub(crate) use accounting::RunAccounting;
pub(crate) use step_loop::{FirstInput, Halt, LoopConfig, PERSIST_FAILED, StepLoop, record_pending_entries};
pub(crate) use trace::{EventTarget, RunTrace};

/// Bounds for one run. Defaults impose no request or time limit.
/// Continuation is supplied by a finish_step hook.
#[derive(Clone, Debug, Default)]
pub struct RunLimits {
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
    /// ([`AgentConfig::models`](crate::AgentConfig::models)), summary requests and
    /// failed attempts included. Checked before each request, so the last one may go over.
    /// With a budget, every request's final model must have prices; a missing price fails
    /// the run before dispatch. Tool usage is not counted. `None` disables the budget.
    pub cost_budget: Option<f64>,
}

/// What links a run to its reader: the run's ID, the token that cancels it, and where the
/// reader takes its events from.
pub(crate) struct RunLink {
    pub(crate) id: String,
    pub(crate) cancel: CancellationToken,
    pub(crate) events: mpsc::Sender<Event>,
}

/// Runs prompts against one session, adding retries, limits, and persistence to the loop.
/// One Runner owns one conversation, and `&mut self` keeps its runs from overlapping.
pub(crate) struct Runner {
    config: LoopConfig,
    retry: RetryPolicy,
    limits: RunLimits,
    transcript: Transcript,
    /// Shared with the session's handles, which answer the requests that wait there.
    approvals: Arc<ApprovalGate>,
    compaction: compaction::Config,
    pause: CompactionPause,
}

impl Runner {
    /// Create a runner. Each step of a run is recorded in `session` before it takes effect;
    /// its events follow.
    pub(crate) fn new(config: LoopConfig, retry: RetryPolicy, limits: RunLimits, transcript: Transcript) -> Self {
        Self {
            config,
            retry,
            limits,
            transcript,
            approvals: Arc::default(),
            compaction: compaction::Config::default(),
            pause: CompactionPause::default(),
        }
    }

    /// Opt into automatic context preparation or tune manual summary budgets.
    /// Rejects invalid budgets without making requests or changing the session.
    pub(crate) fn with_compaction(mut self, config: compaction::Config) -> anyhow::Result<Self> {
        config.validate()?;
        self.compaction = config;
        Ok(self)
    }

    /// Summarize the old prefix without executing a task or tools, as the run `link` names.
    /// Cancellation and future drop cancel only this operation.
    pub(crate) async fn compact(&mut self, link: &RunLink) -> compaction::Report {
        let child = link.cancel.child_token();
        let _cancel_on_drop = child.drop_guard_ref();
        let trace = self.config.trace(&self.transcript, link.id.clone(), child.clone());
        let target = EventTarget {
            events: &link.events,
            trace: &trace,
            hooks: &self.config.hooks,
        };
        let run_start = self.transcript.entries().len();
        let mut host = RunHost {
            transcript: &mut self.transcript,
            run_start,
            target,
            config: &self.config,
            compaction: &self.compaction,
            pause: &mut self.pause,
            cost_budget: self.limits.cost_budget,
        };
        host.compact(Trigger::Manual, 1).await.into()
    }

    /// The loop configuration.
    pub(crate) fn config(&self) -> &LoopConfig {
        &self.config
    }

    /// Where the calls of this conversation wait for approval.
    pub(crate) fn approvals(&self) -> &Arc<ApprovalGate> {
        &self.approvals
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
    pub(crate) fn transcript(&self) -> &Transcript {
        &self.transcript
    }

    /// The session, for what a host does to it between runs: a branch switch, or a record
    /// of its own, such as an extension's state.
    pub(crate) fn transcript_mut(&mut self) -> &mut Transcript {
        &mut self.transcript
    }

    /// Run from `begin` to completion, as the run `link` names: a prompt, or the session as
    /// it is ([`Session::continue_run`](crate::session::Session::continue_run) says what is
    /// then answered). Never panics on provider or tool failures; the report says how the
    /// run ended.
    pub(crate) async fn run(&mut self, begin: Start, link: &RunLink) -> RunReport {
        let run_cancel = link.cancel.child_token();
        // Dropping this future must cancel background providers even when the session
        // keeps the run's token alive.
        let _cancel_on_drop = run_cancel.drop_guard_ref();
        let timer = RunTimer::start(self.limits.timeout, &run_cancel);
        let Self {
            config,
            retry,
            limits,
            transcript,
            approvals,
            compaction,
            pause,
        } = self;
        let trace = config.trace(transcript, link.id.clone(), run_cancel.clone());
        let target = EventTarget {
            events: &link.events,
            trace: &trace,
            hooks: &config.hooks,
        };
        // The run's answer is what it adds after the entries the session has now. Counted in
        // entries, whose IDs remain stable even when hooks omit messages from earlier runs.
        let run_start = transcript.entries().len();
        let running = Running {
            host: RunHost {
                transcript,
                run_start,
                target,
                config,
                compaction,
                pause,
                cost_budget: limits.cost_budget,
            },
            retry,
            limits,
            approvals,
            accounting: RunAccounting::new(limits.max_model_requests, limits.token_budget, limits.cost_budget),
            overflow_recovered: false,
            timer,
        };
        running.run(begin).await
    }
}

/// One run in progress: the host it records through, what it borrows from the
/// Runner, and what its attempts have spent so far.
struct Running<'a> {
    host: RunHost<'a>,
    retry: &'a RetryPolicy,
    limits: &'a RunLimits,
    approvals: &'a ApprovalGate,
    accounting: RunAccounting,
    /// A context overflow was recovered from once already.
    overflow_recovered: bool,
    timer: RunTimer,
}

impl Running<'_> {
    /// Open the run from `begin`, run its attempts, and report how it ended.
    async fn run(mut self, begin: Start) -> RunReport {
        let stop = match self.host.open_run(begin).await {
            Ok(first) => self.attempts(first).await,
            Err(error) => RunStop::Failed(error),
        };
        self.close(stop).await
    }

    /// Make an attempt that starts with `first`, then retry or recover from a context
    /// overflow as long as the budget and the retry policy allow, until an attempt ends the
    /// run. An attempt after the first starts with the conversation as it is.
    async fn attempts(&mut self, mut first: FirstInput) -> RunStop {
        loop {
            let steps_before = self.accounting.task_steps;
            let (failed, entry_id) = match self
                .attempt(std::mem::replace(&mut first, FirstInput::Conversation))
                .await
            {
                Halt::Ended(ending) => break RunStop::Ended(ending),
                // Hook and recording failures are distinct from failed provider responses and
                // are not retried.
                Halt::Failed(error) => break RunStop::Failed(error),
                Halt::Unrecorded(error) => break RunStop::Failed(format!("{error:#}")),
                Halt::ProviderFailed { message, entry_id } => (message, entry_id),
            };
            let task_steps = self.accounting.task_steps - steps_before;
            if let Err(stop) = self.after_failure(&failed, &entry_id, task_steps).await {
                break stop;
            }
        }
    }

    /// Make one attempt: run the loop, which counts what it spends into the run's
    /// accounting and checks the run's budgets. Returns why the loop stopped.
    async fn attempt(&mut self, first: FirstInput) -> Halt {
        self.accounting.attempts = self.accounting.attempts.saturating_add(1);
        // The host borrowed again for this attempt alone, so that the loop's borrow of the
        // host and the host's borrow of the Runner end together.
        StepLoop::new(self.host.reborrow(), self.approvals, &mut self.accounting)
            .run(first)
            .await
    }

    /// Close the run: keep the state hooks asked to record, then report how the run ended
    /// and the answer it added.
    async fn close(self, stop: RunStop) -> RunReport {
        // State that hooks asked to record during the last events is kept before the run is
        // reported, so a host reading the session afterwards finds it.
        let (config, target) = (self.host.config, self.host.target);
        let stop = match record_pending_entries(&config.queues, self.host.transcript, target).await {
            Ok(()) => stop,
            Err(error) => RunStop::Failed(format!("{error:#}")),
        };
        let transcript = &self.host.transcript;
        let added = transcript.messages_since(self.host.run_start);
        let (outcome, error) = self.conclude(stop, cut_off(&added));
        let accounting = &self.accounting;
        let report = RunReport {
            outcome,
            final_text: final_answer(&added),
            model_requests: accounting.model_requests,
            retries: accounting.retries,
            usage: accounting.usage.clone(),
            tool_usage: accounting.tool_usage.clone(),
            denied_calls: accounting.denied_calls,
            error,
            session_id: transcript.header().id.clone(),
        };
        target
            .emit(EventKind::RunEnd {
                report: Box::new(report.clone()),
            })
            .await;
        report
    }

    /// How the run ended, as the report states it; `cut_off` says whether its last answer
    /// was cut off at the output token limit.
    fn conclude(&self, stop: RunStop, cut_off: bool) -> (RunOutcome, Option<String>) {
        let (limits, accounting, timed_out) = (self.limits, &self.accounting, self.timer.timed_out());
        let ending = match stop {
            RunStop::Failed(error) => return (RunOutcome::Failed, Some(error)),
            RunStop::Ended(ending) => ending,
        };
        match ending {
            Ending::Completed => (RunOutcome::Completed, None),
            Ending::Truncated => (
                RunOutcome::Truncated,
                Some("the answer was cut off at the output token limit".to_string()),
            ),
            Ending::BudgetExhausted(budget) => {
                let reached = match budget {
                    Budget::ModelRequests => format!(
                        "model request limit reached (max_model_requests = {})",
                        limits
                            .max_model_requests
                            .expect("a request budget ends a run only when one is configured")
                    ),
                    Budget::Tokens => format!(
                        "token budget reached ({} tokens used, token_budget = {})",
                        accounting.usage.input_and_output(),
                        limits
                            .token_budget
                            .expect("a token budget ends a run only when one is configured")
                    ),
                    Budget::Cost => format!(
                        "cost budget reached ({:.4} spent, cost_budget = {})",
                        accounting.usage.total_cost.unwrap_or(0.0),
                        limits
                            .cost_budget
                            .expect("a cost budget ends a run only when one is configured")
                    ),
                };
                let error = if cut_off {
                    format!("{reached}; the answer was cut off at the output token limit")
                } else {
                    reached
                };
                (ending.outcome(cut_off), Some(error))
            }
            // The time limit cancels the attempt, and only `timed_out` tells that
            // cancellation from the caller's.
            Ending::Cancelled if timed_out => {
                // Only the timer of a configured limit sets `timed_out`.
                let limit = limits.timeout.expect("Timeout requires a configured time limit");
                (
                    RunOutcome::TimedOut,
                    Some(format!("run exceeded the {}s time limit", limit.as_secs())),
                )
            }
            Ending::Cancelled => (RunOutcome::Cancelled, Some("run was cancelled".to_string())),
        }
    }
}

/// How an attempt, and with it the run, ends when nothing failed. A failure is not among
/// these: it comes with its reason ([`Halt::Failed`], [`RunStop::Failed`]). Nor is the time
/// limit: it cancels the run, and the run's timer tells that from the caller's cancellation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ending {
    Completed,
    /// The answer was cut off at the output token limit.
    Truncated,
    /// The budget left no room for another request.
    BudgetExhausted(Budget),
    Cancelled,
}

impl Ending {
    /// The outcome as events and reports name it; `cut_off` says whether the run's last
    /// answer was cut off at the output token limit.
    pub(crate) fn outcome(self, cut_off: bool) -> RunOutcome {
        match self {
            Self::Completed => RunOutcome::Completed,
            Self::Truncated => RunOutcome::Truncated,
            Self::BudgetExhausted(budget) => RunOutcome::BudgetExhausted {
                budget,
                truncated: cut_off,
            },
            Self::Cancelled => RunOutcome::Cancelled,
        }
    }
}

/// What a run starts from.
pub(crate) enum Start {
    /// The person's text, or a message from an extension.
    Message(Input),
    /// The session as it is.
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
    use crate::ai::Message;
    use crate::ai::faux::{FauxProvider, FauxStep};
    use crate::tools::{Tool, Toolbox};
    use crate::transcript::{FailingWriter, SessionHeader};

    #[test]
    fn default_step_budget_is_unbounded() {
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
    async fn compaction_write_failures_cancel_before_any_further_model_request() {
        for rejected in [
            "compaction_started",
            "request_config",
            "request_started",
            "compaction",
            "compaction_failed",
        ] {
            let header = SessionHeader::new(Path::new("."), "m");
            let mut store = Transcript::with_writer(header, RejectWrites::entry(rejected));
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
        let mut store = Transcript::with_writer(header, RejectWrites::entry("compaction_failed"));
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

        let report = run(&mut runner, "continue", &cancel).await;

        assert_eq!(report.outcome, RunOutcome::Failed, "{report:?}");
        assert!(report.error.unwrap().contains("disk full on compaction_failed"));
    }

    #[tokio::test]
    async fn a_session_write_failure_cancels_the_run() {
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
    async fn a_recovery_write_failure_cancels_the_run_before_any_request() {
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
        #[async_trait::async_trait]
        impl crate::hooks::Hook for Rewrite {
            async fn on_input(
                &self,
                _text: String,
                _ctx: &crate::hooks::HookContext,
            ) -> crate::hooks::HookResult<String> {
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
}
