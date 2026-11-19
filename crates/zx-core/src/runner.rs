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
//! - `run_scope`: what one run works with across its attempts, which the loop and the
//!   compactor work through, how a run opens, and when its context is summarized
//! - `failure`: what follows a provider failure: a retry, or one recovery from a context
//!   the model rejected as too long
//! - `interrupted`: results for the tool calls an interrupted run left unfinished
//! - `final_answer`: the answer, joined from the parts that continue a cut-off one
//! - `tests`, compiled for tests only: how a session that cannot be written stops a run
//!   before it acts

mod accounting;
mod failure;
mod final_answer;
mod interrupted;
mod run_scope;
mod step_loop;
mod trace;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use final_answer::{cut_off, final_answer};
use run_scope::{CompactionPause, RunScope, RunStop};

use crate::ai::retry::RetryPolicy;
use crate::ai::{Provider, Reasoning};
use crate::compaction::{self, Trigger};
use crate::events::{Budget, Event, EventKind, RunOutcome, RunReport};
use crate::input::PendingInput;
use crate::policy::ApprovalGate;
use crate::sync::AbortOnDrop;
use crate::transcript::Transcript;
pub(crate) use accounting::RunAccounting;
use run_scope::PERSIST_FAILED;
pub(crate) use step_loop::{FirstInput, Halt, LoopConfig, StepLoop};
pub(crate) use trace::{EventTarget, RunTrace};

/// Bounds for one run. The defaults impose no request limit, time limit or budget.
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
    /// Create a runner. Each step of a run is recorded in `transcript` before it takes
    /// effect; its events follow.
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

    /// Opt into automatic context preparation or tune manual summary budgets, which the
    /// agent checked when it was made ([`compaction::Config::validate`]).
    pub(crate) fn with_compaction(mut self, config: compaction::Config) -> Self {
        self.compaction = config;
        self
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
        let mut scope = RunScope {
            transcript: &mut self.transcript,
            run_start,
            target,
            config: &self.config,
            compaction: &self.compaction,
            pause: &mut self.pause,
            cost_budget: self.limits.cost_budget,
        };
        scope.compact(Trigger::Manual, 1).await.into()
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

    /// Ask later task requests for `reasoning`; `None` leaves the server's default.
    pub(crate) fn set_reasoning(&mut self, reasoning: Option<Reasoning>) {
        self.config.reasoning = reasoning;
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
            scope: RunScope {
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

/// One run in progress: the scope it records through, what else it borrows from the
/// Runner, and what its attempts have spent so far.
struct Running<'a> {
    scope: RunScope<'a>,
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
        let stop = match self.scope.open(begin).await {
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
        // The scope borrowed again for this attempt alone, so that the loop's borrow of the
        // scope and the scope's borrow of the Runner end together.
        StepLoop::new(self.scope.reborrow(), self.approvals, &mut self.accounting)
            .run(first)
            .await
    }

    /// Close the run: keep the state hooks asked to record, then report how the run ended
    /// and the answer it added.
    async fn close(mut self, stop: RunStop) -> RunReport {
        // State that hooks asked to record during the last events is kept before the run is
        // reported, so a host reading the session afterwards finds it.
        let stop = match self.scope.record_pending_entries().await {
            Ok(()) => stop,
            Err(error) => RunStop::Failed(format!("{error:#}")),
        };
        let transcript = &self.scope.transcript;
        let added = transcript.messages_since(self.scope.run_start);
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
        self.scope
            .target
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
                let limit = limits
                    .timeout
                    .expect("a timeout ends a run only when a time limit is configured");
                (
                    RunOutcome::TimedOut,
                    Some(format!("run exceeded the {limit:?} time limit")),
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
    /// The person's text, or a message from an extension, with the host's hold.
    Message(PendingInput),
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
mod tests;
