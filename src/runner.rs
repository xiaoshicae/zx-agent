//! Run-level control around the loop (a small subset of pi's coding-agent AgentSession).
//!
//! The loop stops at the first provider error; this layer decides whether to retry, keeps
//! the session as the source of model context, and enforces turn and time limits.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::agent::{
    AgentEvent, ContextPreparation, Effects, EventSink, HookChain, Interruption, LoopConfig, LoopControl, LoopFailure,
    LoopOutcome, SessionHost, run_loop,
};
use crate::ai::retry::{RetryPolicy, is_retryable};
use crate::ai::{Message, Provider, Request, ToolCall, ToolResultMessage, Usage, UserMessageKind, duration_ms, now_ms};
use crate::compaction::{self, Trigger};
use crate::interaction::ApprovalCache;
use crate::session::{Entry, RequestSource, SessionStore, StartRecord};
use crate::sync::AbortOnDrop;

/// Bounds for one run. Defaults impose no turn or time limit.
/// Continuation is supplied by a finish_turn hook.
#[derive(Clone, Debug, Default)]
pub struct Limits {
    /// Model requests per run, failed attempts included. `None` disables the limit;
    /// `Some(0)` prevents any model request.
    pub max_turns: Option<u32>,
    /// Wall-clock limit for the whole run; `None` disables it.
    pub timeout: Option<Duration>,
}

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    /// The model gave a final answer.
    Completed,
    /// The final answer stayed cut off at the output token limit after the allowed
    /// continuations. `final_text` holds what was produced.
    Truncated,
    /// A request exhausted retries, an operational hook failed, or persistence failed.
    Error,
    /// The turn budget ran out while the model still wanted to continue.
    MaxTurns,
    /// The wall-clock limit was reached.
    Timeout,
    /// The caller cancelled the run.
    Aborted,
}

/// Summary of a finished run.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunReport {
    pub outcome: RunOutcome,
    /// Text of the last assistant message in the session projection, joined with the parts
    /// it continues when earlier responses were cut off at the output token limit.
    pub final_text: String,
    /// Model requests made, saturating at `u32::MAX` for unbounded runs.
    pub turns: u32,
    pub retries: u32,
    /// Usage of every response in this run, failed attempts included. A cancelled request
    /// counts as zero: servers report usage only at the end of the stream.
    pub usage: Usage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
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
    /// Runner borrow as run; cancellation and future drop cancel only this operation.
    pub async fn compact(&mut self, cancel: &CancellationToken) -> compaction::Report {
        let child = cancel.child_token();
        let _cancel_on_drop = child.drop_guard_ref();
        let mut host = compaction::Host {
            session: &mut self.session,
            provider: self.config.spec.provider.as_ref(),
            model: &self.config.spec.model,
            config: &self.compaction,
            sink: self.sink.as_ref(),
            hooks: &self.config.hooks,
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
    /// use zx_agent::runner::Runner;
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

        // Entry IDs remain stable even when hooks omit messages from earlier runs.
        let run_start = self.session.entries().len();
        // Before any system prompt change or input, so that results directly follow their calls.
        let recovered = self.recover_interrupted_calls(&run_cancel).await;
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
        let mut prompts = vec![Message::user(prompt)];
        let mut turns: u32 = 0;
        let mut retries = 0;
        let mut usage = Usage::default();
        let mut last_error = None;
        let mut continuations = 0;

        let ending = match recovered {
            Err(error) => {
                last_error = Some(error);
                LoopOutcome::Error
            }
            Ok(()) => loop {
                // The session projection, not an in-memory list, is what the model sees
                // (pi: AgentSession replaces the context with the projection before each request).
                let mut context = session.messages();
                let mut host = RunnerHost {
                    session: &mut *session,
                    sink,
                    hooks: &config.hooks,
                    provider: config.spec.provider.as_ref(),
                    model: &config.spec.model,
                    compaction,
                    compaction_pause: &mut *compaction_pause,
                };
                let mut control = LoopControl::new(
                    limits.max_turns.map(|limit| limit.saturating_sub(turns)),
                    &mut *approvals,
                    &run_cancel,
                    sink,
                );
                control.turn_offset = turns;
                control.continuations = continuations;
                control.host = Some(&mut host);
                let result = run_loop(&mut context, std::mem::take(&mut prompts), config, &mut control).await;
                turns = turns.saturating_add(result.turns);
                continuations = result.continuations;
                usage.add(&result.usage);
                if result.outcome != LoopOutcome::Error {
                    break result.outcome;
                }
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
                let retryable = is_retryable(&failed);
                if !retryable || retries >= retry.max_retries || run_cancel.is_cancelled() {
                    break LoopOutcome::Error;
                }

                retries += 1;
                let delay = retry.delay_for(retries, &failed);
                config
                    .hooks
                    .emit(
                        AgentEvent::RetryScheduled {
                            attempt: retries,
                            max_retries: retry.max_retries,
                            delay_ms: duration_ms(delay),
                            error: last_error.clone().unwrap_or_default(),
                        },
                        &run_cancel,
                        sink,
                    )
                    .await;
                // Keep the failed attempt in the session but drop it from model context, then
                // continue from the previous message (pi: AgentSession._prepareRetry and
                // _omitRecoveryAttempt). No user message is repeated.
                if let Some(entry_id) = entry_id
                    && let Err(error) = session.append_context_edit(&entry_id, None).await
                {
                    last_error = Some(format!("cannot omit the failed attempt: {error:#}"));
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
            LoopOutcome::MaxTurns => (
                RunOutcome::MaxTurns,
                Some(format!(
                    "turn limit reached (max_turns = {})",
                    limits.max_turns.expect("MaxTurns requires a configured turn limit")
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
        RunReport {
            outcome,
            final_text,
            turns,
            retries,
            usage,
            error,
            session_file: session.path().map(|p| p.display().to_string()),
        }
    }

    /// Record an error result for each call that an interrupted run left without one, so
    /// the next request cannot suggest that such a call did nothing. Calls are never run
    /// again. Stops at the first result the session cannot record.
    async fn recover_interrupted_calls(&mut self, cancel: &CancellationToken) -> Result<(), String> {
        let Self {
            config, session, sink, ..
        } = self;
        for call in session.interrupted_tool_calls() {
            let effects = config
                .spec
                .tools
                .iter()
                .find(|tool| tool.name() == call.tool_name)
                .map(|tool| tool.effects());
            let state = interruption(call.start, effects);
            let event = AgentEvent::ToolCallRecovered {
                tool_call_id: call.tool_call_id.clone(),
                tool_name: call.tool_name.clone(),
                state,
            };
            config.hooks.emit(event, cancel, sink.as_ref()).await;
            let result = Message::ToolResult(ToolResultMessage {
                tool_call_id: call.tool_call_id,
                tool_name: call.tool_name,
                content: recovered_result(state).to_string(),
                is_error: true,
                timestamp: now_ms(),
                duration_ms: None,
            });
            let start = AgentEvent::MessageStart { role: "toolResult" };
            config.hooks.emit(start, cancel, sink.as_ref()).await;
            session
                .append_message(result.clone())
                .await
                .map_err(|error| format!("cannot persist session entry: {error:#}"))?;
            let end = AgentEvent::MessageEnd { message: result };
            config.hooks.emit(end, cancel, sink.as_ref()).await;
        }
        Ok(())
    }
}

/// What recovery may claim about an interrupted call. A start record means the call may
/// have taken effect; its absence proves nothing for read-only tools, which are never
/// recorded, or for tools that are no longer configured, whose effects are unknown.
fn interruption(start: StartRecord, effects: Option<Effects>) -> Interruption {
    match (start, effects) {
        (StartRecord::Present, _) => Interruption::Started,
        (StartRecord::Absent | StartRecord::Unavailable, Some(Effects::ReadOnly)) => Interruption::ReadOnly,
        (StartRecord::Absent, Some(Effects::SideEffects)) => Interruption::NotStarted,
        (StartRecord::Unavailable, Some(Effects::SideEffects))
        | (StartRecord::Absent | StartRecord::Unavailable, None) => Interruption::StartUnknown,
    }
}

/// The result the model sees for an interrupted call.
fn recovered_result(state: Interruption) -> &'static str {
    match state {
        Interruption::Started => {
            "Result unknown: this call started, but the agent stopped before its result was recorded. It may have changed state; check the current state before running it again."
        }
        Interruption::StartUnknown => {
            "Result unknown: the agent stopped before this call's result was recorded, and whether the call started was not recorded. It may have changed state; check the current state before running it again."
        }
        Interruption::NotStarted => {
            "Not executed: the agent stopped before this call started, so it had no effect. Run it again if it is still needed."
        }
        Interruption::ReadOnly => {
            "No result: the agent stopped before this read-only call finished. It changed nothing; run it again if the result is still needed."
        }
    }
}

/// Join explicit continuation fragments, ignoring intervening system prompt updates.
fn final_answer(messages: &[Message]) -> String {
    let mut parts = Vec::new();
    let mut end = messages.len();
    while let Some((index, answer)) =
        messages[..end]
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, message)| match message {
                Message::Assistant(answer) => Some((index, answer)),
                Message::System(_) | Message::User { .. } | Message::ToolResult(_) => None,
            })
    {
        parts.push(answer.text());
        let mut preceding = messages[..index]
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, message)| !matches!(message, Message::System(_)));
        match (preceding.next(), preceding.next()) {
            (
                Some((
                    _,
                    Message::User {
                        kind: UserMessageKind::ContinueAnswer,
                        ..
                    },
                )),
                Some((previous, Message::Assistant(_))),
            ) => end = previous + 1,
            _ => break,
        }
    }
    parts.reverse();
    let mut joined = String::new();
    for part in &parts {
        // Models often restart the cut-off line instead of continuing it.
        let repeated = overlap(&joined, part);
        joined.push_str(&part[repeated..]);
    }
    joined
}

/// Length in bytes of the longest end of `previous` that `next` starts with. Matches shorter
/// than 8 bytes are treated as coincidence, and only the last 1000 bytes are searched.
fn overlap(previous: &str, next: &str) -> usize {
    let longest = previous.len().min(next.len()).min(1000);
    (8..=longest)
        .rev()
        .find(|&len| next.is_char_boundary(len) && previous.ends_with(&next[..len]))
        .unwrap_or(0)
}

/// Records each step of a run in the session before the loop acts on it (pi: AgentSession
/// writes on `message_end`), and prepares the context before each task request. A failed
/// write stops the run: an unattended run must not keep acting without an audit trail.
struct RunnerHost<'a> {
    session: &'a mut SessionStore,
    sink: &'a dyn EventSink,
    hooks: &'a HookChain,
    provider: &'a dyn Provider,
    model: &'a str,
    compaction: &'a compaction::Config,
    /// Borrowed from the Runner, so the pause outlives one prompt and a dropped run.
    compaction_pause: &'a mut Option<String>,
}

#[async_trait]
impl SessionHost for RunnerHost<'_> {
    async fn append_message(&mut self, message: &Message) -> Result<String, String> {
        // The event that reports the message owns it, so the session stores a copy.
        self.session
            .append_message(message.clone())
            .await
            .map_err(|error| format!("cannot persist session entry: {error:#}"))
    }

    async fn edit_context(&mut self, index: usize, replacement: Option<&str>) -> Result<(), String> {
        let projection = self.session.projection();
        let target = projection
            .get(index)
            .ok_or_else(|| format!("context index {index} is out of bounds"))?;
        self.session
            .append_context_edit(&target.entry_id, replacement.map(str::to_string))
            .await
            .map(|_| ())
            .map_err(|error| format!("cannot persist context edit: {error:#}"))
    }

    async fn record_request(&mut self, request: &Request, provider: &dyn Provider) -> Result<(), String> {
        let options = provider.request_options();
        self.session
            .record_request(request, provider.name(), options, RequestSource::Projection)
            .await
            .map(|_| ())
            .map_err(|error| format!("cannot persist request checkpoint: {error:#}"))
    }

    async fn record_tool_call_started(&mut self, call: &ToolCall) -> Result<(), String> {
        self.session
            .append_tool_call_started(&call.id, &call.name)
            .await
            .map(|_| ())
            .map_err(|error| format!("cannot persist the start of tool call {:?}: {error:#}", call.id))
    }

    async fn prepare_context(&mut self, cancel: &CancellationToken) -> ContextPreparation {
        let Some(limit) = self.compaction.trigger_bytes else {
            return ContextPreparation::Unchanged;
        };
        match compaction::context_bytes(&self.session.messages()) {
            Ok(bytes) if bytes <= limit => return ContextPreparation::Unchanged,
            Ok(_) => {}
            Err(error) => {
                return ContextPreparation::Failed {
                    error: format!("cannot measure context: {error:#}"),
                    requests: 0,
                    usage: Usage::default(),
                };
            }
        }
        if let Some(reason) = self.compaction_pause.as_deref() {
            return ContextPreparation::Failed {
                error: format!(
                    "automatic compaction is paused because it failed earlier in this session ({reason}); \
                     compact manually to retry, start a new session, or disable automatic compaction"
                ),
                requests: 0,
                usage: Usage::default(),
            };
        }
        let mut host = compaction::Host {
            session: &mut *self.session,
            provider: self.provider,
            model: self.model,
            config: self.compaction,
            sink: self.sink,
            hooks: self.hooks,
        };
        let (report, messages) = host.run(Trigger::Automatic, cancel).await;
        if let Some(error) = report.persistence_error {
            return ContextPreparation::Unrecorded {
                error,
                requests: report.requests,
                usage: report.usage,
            };
        }
        match messages {
            Some(messages) => ContextPreparation::Updated {
                messages,
                requests: report.requests,
                usage: report.usage,
            },
            None => {
                let error = report
                    .error
                    .unwrap_or_else(|| "automatic compaction made no progress".into());
                // A paid summary request failed; repeating it on every prompt would keep
                // paying for the same failure. Cancellation is the caller's choice.
                if report.outcome == compaction::Outcome::Failed && report.requests > 0 {
                    *self.compaction_pause = Some(error.clone());
                }
                ContextPreparation::Failed {
                    error,
                    requests: report.requests,
                    usage: report.usage,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::agent::{LoopSpec, NullSink};
    use crate::ai::SystemPrompt;
    use crate::ai::faux::{FauxProvider, FauxStep};
    use crate::session::{FailingWriter, SessionHeader};

    #[test]
    fn default_turn_budget_is_unbounded() {
        assert_eq!(Limits::default().max_turns, None);
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
            async fn on_input(&self, _text: String) -> crate::agent::HookResult<String> {
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
