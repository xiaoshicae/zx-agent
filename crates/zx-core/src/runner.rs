//! Run-level control around the loop (a small subset of pi's coding-agent AgentSession).
//!
//! The loop stops at the first provider error; this layer decides whether to retry, keeps
//! the session as the source of model context, and enforces request, time and token limits.

mod accounting;
mod answer;
mod overflow;
mod recovery;
mod session_runtime;

use accounting::RunAccounting;
use answer::final_answer;
#[cfg(test)]
use answer::overlap;
use overflow::OverflowRecovery;
use session_runtime::RunnerSessionRuntime;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::agent::{
    AgentEvent, Emit, EventSink, HookChain, LoopConfig, LoopControl, LoopFailure, LoopOutcome, RunTrace, run_loop,
};
pub use crate::agent::{RunOutcome, RunReport};
use crate::ai::retry::{RetryPolicy, is_context_overflow, is_retryable};
use crate::ai::{Message, Provider, duration_ms};
use crate::compaction::{self, Trigger};
use crate::interaction::ApprovalCache;
use crate::session::{Entry, SessionStore};
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
    /// Why automatic compaction is paused after a paid summary request failed. Kept across
    /// prompts so the same failing summary is not paid for again on every prompt. A run only
    /// borrows it, so the pause also survives a run whose future is dropped.
    compaction_pause: Option<String>,
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
            compaction_pause: None,
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
        let trace = RunTrace::new(
            Some(self.session.header().id.clone()),
            self.config.spec.cwd.clone(),
            child.clone(),
        );
        let mut host = compaction::Host {
            session: &mut self.session,
            provider: self.config.spec.provider.as_ref(),
            model: &self.config.spec.model,
            config: &self.compaction,
            to: Emit {
                sink: self.sink.as_ref(),
                trace: &trace,
            },
            hooks: &self.config.hooks,
            attempt: 1,
        };
        let report = host.run(Trigger::Manual, &child).await.0;
        if report.outcome == compaction::Outcome::Completed {
            // A summary succeeded, so automatic attempts may succeed again.
            self.compaction_pause = None;
        }
        report
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
        self.compaction_pause = None;
        (
            std::mem::replace(&mut self.config.spec.provider, provider),
            std::mem::replace(&mut self.config.spec.model, model),
        )
    }

    /// Revoke exact-call approvals granted by this Runner's interaction host.
    pub fn clear_approvals(&mut self) {
        self.approvals.clear();
    }

    /// The session.
    pub fn session(&self) -> &SessionStore {
        &self.session
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
        let run_cancel = cancel.child_token();
        // Dropping this future must cancel background providers even when the caller
        // keeps its parent token alive.
        let _cancel_on_drop = run_cancel.drop_guard_ref();
        let timed_out = Arc::new(AtomicBool::new(false));
        let _timer = self.limits.timeout.map(|limit| {
            let token = run_cancel.clone();
            let flag = timed_out.clone();
            AbortOnDrop(tokio::spawn(async move {
                tokio::time::sleep(limit).await;
                flag.store(true, Ordering::SeqCst);
                token.cancel();
            }))
        });

        let trace = RunTrace::new(
            Some(self.session.header().id.clone()),
            self.config.spec.cwd.clone(),
            run_cancel.clone(),
        );
        let start = AgentEvent::RunStart {
            model: self.config.spec.model.clone(),
            provider: self.config.spec.provider.name().into(),
        };
        let to = Emit {
            sink: self.sink.as_ref(),
            trace: &trace,
        };
        self.config.hooks.emit(start, to).await;
        // Entry IDs remain stable even when hooks omit messages from earlier runs.
        let run_start = self.session.entries().len();
        // Before any system prompt change or input, so that results directly follow their calls.
        let recovered = self.recover_interrupted_calls(&trace).await;
        let Self {
            config,
            retry,
            limits,
            session,
            sink,
            approvals,
            compaction,
            compaction_pause,
        } = self;
        let sink = sink.as_ref();
        let to = Emit { sink, trace: &trace };
        let mut prompts = vec![Message::user(prompt)];
        let mut accounting = RunAccounting::default();
        let mut last_error = None;
        let mut overflow_recovered = false;

        let ending = match recovered {
            Err(error) => {
                last_error = Some(error);
                LoopOutcome::Error
            }
            Ok(()) => loop {
                let attempt = accounting.attempts + 1;
                // The session projection, not an in-memory list, is what the model sees
                // (pi: AgentSession replaces the context with the projection before each request).
                let mut context = session.messages();
                let mut session_runtime = RunnerSessionRuntime {
                    session: &mut *session,
                    to,
                    hooks: &config.hooks,
                    provider: config.spec.provider.as_ref(),
                    model: &config.spec.model,
                    compaction,
                    compaction_pause: &mut *compaction_pause,
                    attempt,
                };
                let mut control = LoopControl::new(
                    accounting.remaining_model_requests(limits),
                    &mut *approvals,
                    &run_cancel,
                    sink,
                );
                control.token_budget = accounting.remaining_tokens(limits);
                control.prior_task_turns = accounting.task_turns;
                control.prior_answer_continuations = accounting.answer_continuations;
                control.session = Some(&mut session_runtime);
                control.trace = Some(&trace);
                control.attempt_number = attempt;
                let result = run_loop(&mut context, std::mem::take(&mut prompts), config, &mut control).await;
                accounting.record(&result);
                if result.outcome != LoopOutcome::Error {
                    break result.outcome;
                }
                // More than one turn means an answer succeeded after any earlier recovery.
                let task_turns = result.task_turns;
                // Hook failures are distinct from failed provider responses and are not retried.
                let (failed, entry_id) = match result.failure {
                    Some(LoopFailure::Provider { message, entry_id }) => (message, entry_id),
                    Some(LoopFailure::Host(error)) => {
                        last_error = Some(error);
                        break LoopOutcome::Error;
                    }
                    None => {
                        last_error = Some("loop failed without a diagnostic".into());
                        break LoopOutcome::Error;
                    }
                };
                last_error = failed.error_message.clone();
                // A context the model rejects as too long fails again however often it is
                // sent; with automatic compaction enabled, compact once and continue (pi:
                // AgentSession's overflow recovery). Not a retry: it has its own limit.
                if is_context_overflow(&failed) && compaction.trigger_bytes.is_some() {
                    let error = last_error.clone().unwrap_or_default();
                    let refusal = if overflow_recovered && task_turns <= 1 {
                        Some("the context still overflowed after compacting it once".to_string())
                    } else {
                        compaction_pause.as_deref().map(|reason| {
                            format!(
                                "automatic compaction is paused because it failed earlier in this session ({reason})"
                            )
                        })
                    };
                    if let Some(refusal) = refusal {
                        last_error = Some(format!("{error}; {refusal}"));
                        break LoopOutcome::Error;
                    }
                    // A summary is a request, so it needs budget like a retry does.
                    if !accounting.has_budget(limits) || run_cancel.is_cancelled() {
                        break LoopOutcome::Error;
                    }
                    let recovery = OverflowRecovery {
                        session: &mut *session,
                        hooks: &config.hooks,
                        to,
                        provider: config.spec.provider.as_ref(),
                        model: &config.spec.model,
                        compaction,
                        compaction_pause: &mut *compaction_pause,
                        attempt,
                        cancel: &run_cancel,
                    };
                    match recovery.run(&error, entry_id.as_deref(), &mut accounting).await {
                        Ok(()) => {
                            overflow_recovered = true;
                            continue;
                        }
                        Err((outcome, error)) => {
                            last_error = Some(error);
                            break outcome;
                        }
                    }
                }
                let retryable = is_retryable(&failed);
                // A retry is a request, so it needs budget; without any, the provider error
                // is the reason the run ends.
                let budget_left = accounting.has_budget(limits);
                if !retryable || accounting.retries >= retry.max_retries || !budget_left || run_cancel.is_cancelled() {
                    break LoopOutcome::Error;
                }

                accounting.retries += 1;
                let delay = retry.delay_for(accounting.retries, &failed);
                config
                    .hooks
                    .emit(
                        AgentEvent::RetryScheduled {
                            attempt: accounting.retries,
                            max_retries: retry.max_retries,
                            delay_ms: duration_ms(delay),
                            error: last_error.clone().unwrap_or_default(),
                        },
                        to,
                    )
                    .await;
                // Keep the failed attempt in the session but drop it from model context, then
                // continue from the previous message (pi: AgentSession._prepareRetry and
                // _omitRecoveryAttempt). No user message is repeated.
                if let Some(target) = entry_id
                    && let Err(error) = omit_failed_attempt(session, &config.hooks, to, &target).await
                {
                    last_error = Some(error);
                    break LoopOutcome::Error;
                }
                let cancelled = tokio::select! {
                    _ = tokio::time::sleep(delay) => false,
                    _ = run_cancel.cancelled() => true,
                };
                if cancelled {
                    break LoopOutcome::Aborted;
                }
            },
        };

        let (outcome, error) = match ending {
            LoopOutcome::Truncated => (
                RunOutcome::Truncated,
                Some("the answer was cut off at the output token limit".to_string()),
            ),
            LoopOutcome::Completed => (RunOutcome::Completed, None),
            LoopOutcome::Error => (RunOutcome::Error, last_error),
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
            LoopOutcome::Aborted if timed_out.load(Ordering::SeqCst) => {
                let secs = limits.timeout.map(|t| t.as_secs()).unwrap_or_default();
                (
                    RunOutcome::Timeout,
                    Some(format!("run exceeded the {secs}s time limit")),
                )
            }
            LoopOutcome::Aborted => (RunOutcome::Aborted, Some("run was cancelled".to_string())),
        };

        // An earlier run's answer must not be reported as this run's.
        let new_ids: HashSet<&str> = session.entries()[run_start..]
            .iter()
            .filter_map(|entry| match entry {
                Entry::Message(message) => Some(message.id.as_str()),
                Entry::ContextEdit(_)
                | Entry::FormatUpgrade(_)
                | Entry::CompactionStarted(_)
                | Entry::Compaction(_)
                | Entry::CompactionFailed(_)
                | Entry::RequestConfig(_)
                | Entry::RequestStarted(_)
                | Entry::ToolCallStarted(_) => None,
            })
            .collect();
        let messages: Vec<Message> = session
            .projection()
            .into_iter()
            .filter(|projected| new_ids.contains(projected.entry_id.as_str()))
            .map(|projected| projected.message)
            .collect();
        let final_text = final_answer(&messages);
        let report = RunReport {
            outcome,
            final_text,
            model_requests: accounting.model_requests,
            retries: accounting.retries,
            usage: accounting.usage,
            error,
            session_file: session.path().map(|p| p.display().to_string()),
        };
        let end = AgentEvent::RunEnd {
            report: Box::new(report.clone()),
        };
        config.hooks.emit(end, to).await;
        report
    }
}

/// Keep a failed answer in the session but drop it from the model context, so that the next
/// attempt continues from the message before it without repeating the user message (pi:
/// AgentSession._prepareRetry and _omitRecoveryAttempt).
async fn omit_failed_attempt(
    session: &mut SessionStore,
    hooks: &HookChain,
    to: Emit<'_>,
    target: &str,
) -> Result<(), String> {
    let index = session.projection().iter().position(|item| item.entry_id == target);
    let edit = session
        .append_context_edit(target, None)
        .await
        .map_err(|error| format!("cannot omit the failed attempt: {error:#}"))?;
    if let Some(index) = index {
        let event = AgentEvent::ContextEdited {
            index,
            replacement: None,
            entry_id: Some(edit),
        };
        hooks.emit(event, to).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::agent::{LoopSpec, NullSink};
    use crate::ai::SystemPrompt;
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

    #[test]
    fn a_final_answer_joins_marked_continuations_across_system_messages() {
        let answer = |text: &str| {
            let mut message = crate::ai::AssistantMessage::failure("p", "m", crate::ai::StopReason::Stop, "");
            message.error_message = None;
            message.content = vec![crate::ai::ContentBlock::Text { text: text.into() }];
            Message::Assistant(message)
        };
        let update = Message::System(crate::ai::SystemMessage {
            sections: vec![crate::ai::SectionChange {
                name: "environment".into(),
                content: Some("changed".into()),
            }],
            timestamp: 0,
        });
        let messages = [
            Message::user("go"),
            answer("甲"),
            update,
            Message::continuation("continue"),
            answer("乙"),
        ];

        assert_eq!(final_answer(&messages), "甲乙");
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
            let config = LoopConfig::new(LoopSpec {
                provider: provider.clone(),
                model: "m".into(),
                system_prompt: SystemPrompt::default(),
                tools: vec![],
                cwd: PathBuf::from("."),
                max_tokens: None,
                sequential_tools: false,
            });
            let mut runner = Runner::new(
                config,
                RetryPolicy::default(),
                Limits::default(),
                store,
                Arc::new(NullSink),
            )
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
        let config = LoopConfig::new(LoopSpec {
            provider: provider.clone(),
            model: "m".into(),
            system_prompt: SystemPrompt::default(),
            tools: vec![],
            cwd: PathBuf::from("."),
            max_tokens: None,
            sequential_tools: false,
        });
        let session = SessionStore::with_writer(SessionHeader::new(Path::new("."), "m"), Box::new(FailingWriter));
        let mut runner = Runner::new(
            config,
            RetryPolicy::default(),
            Limits::default(),
            session,
            Arc::new(NullSink),
        );

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
            let config = LoopConfig::new(LoopSpec {
                provider: provider.clone(),
                model: "m".into(),
                system_prompt: SystemPrompt::default(),
                tools: vec![],
                cwd: PathBuf::from("."),
                max_tokens: None,
                sequential_tools: false,
            });
            let session =
                SessionStore::with_writer(SessionHeader::new(Path::new("."), "m"), Box::new(RejectEntry(rejected)));
            let mut runner = Runner::new(
                config,
                RetryPolicy::default(),
                Limits::default(),
                session,
                Arc::new(NullSink),
            );

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
    impl crate::agent::Tool for Counted {
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

    fn counted_runner(tool: &Arc<Counted>, provider: &Arc<FauxProvider>, session: SessionStore) -> Runner {
        let tool: Arc<dyn crate::agent::Tool> = tool.clone();
        let config = LoopConfig::new(LoopSpec {
            provider: provider.clone(),
            model: "m".into(),
            system_prompt: SystemPrompt::default(),
            tools: vec![tool],
            cwd: PathBuf::from("."),
            max_tokens: None,
            sequential_tools: false,
        });
        Runner::new(
            config,
            RetryPolicy::default(),
            Limits::default(),
            session,
            Arc::new(NullSink),
        )
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
            let mut runner = counted_runner(&tool, &provider, session);

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
        let mut runner = counted_runner(&tool, &provider, session);

        let report = runner.run("continue", &CancellationToken::new()).await;

        assert_eq!(report.outcome, RunOutcome::Error);
        let error = report.error.unwrap_or_default();
        assert!(error.contains("disk full on a tool result"), "{error}");
        assert!(provider.requests().is_empty(), "requested without a recorded result");
        assert_eq!(tool.0.load(Ordering::SeqCst), 0, "recovery must not run calls again");
    }

    #[test]
    fn short_overlaps_are_treated_as_coincidence() {
        assert_eq!(overlap("step 1", "1 more"), 0);
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
        let mut config = LoopConfig::new(LoopSpec {
            provider: provider.clone(),
            model: "m".into(),
            system_prompt: SystemPrompt::default(),
            tools: vec![],
            cwd: PathBuf::from("."),
            max_tokens: None,
            sequential_tools: false,
        });
        config.hooks.push(Rewrite);
        let session = SessionStore::with_writer(SessionHeader::new(Path::new("."), "m"), Box::new(EditFailure));
        let mut runner = Runner::new(
            config,
            RetryPolicy::default(),
            Limits::default(),
            session,
            Arc::new(NullSink),
        );
        let report = runner.run("original", &CancellationToken::new()).await;
        assert_eq!(report.outcome, RunOutcome::Error);
        assert!(report.error.unwrap().contains("disk full on edit"));
        assert!(provider.requests().is_empty());
        assert_eq!(runner.session().messages().len(), 1);
        assert!(matches!(&runner.session().messages()[0], Message::User { content, .. } if content == "original"));
    }

    #[test]
    fn overlaps_are_measured_in_bytes_on_char_boundaries() {
        // "端口错误" is 12 bytes, so it counts even though it is only 4 characters.
        assert_eq!(overlap("数据库端口错误", "端口错误: 5433"), "端口错误".len());
    }
}
