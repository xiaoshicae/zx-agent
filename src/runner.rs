//! Run-level control around the loop (a small subset of pi's coding-agent AgentSession).
//!
//! The loop stops at the first provider error; this layer decides whether to retry, keeps
//! the session file as the source of model context, and enforces turn and time limits.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::agent::{AgentEvent, EventSink, LoopConfig, LoopControl, LoopFailure, LoopOutcome, run_loop};
use crate::ai::retry::{RetryPolicy, is_retryable};
use crate::ai::{Message, Usage, UserMessageKind, duration_ms};
use crate::interaction::ApprovalCache;
use crate::session::{Entry, SessionStore};
use crate::sync::{AbortOnDrop, lock};

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
    session: Mutex<SessionStore>,
    sink: Arc<dyn EventSink>,
    approvals: ApprovalCache,
}

impl Runner {
    /// Create a runner. Events go to `sink` after each finished message is persisted.
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
            session: Mutex::new(session),
            sink,
            approvals: ApprovalCache::default(),
        }
    }

    /// The loop configuration, for example to reach `config().queues` during a run.
    pub fn config(&self) -> &LoopConfig {
        &self.config
    }

    /// Revoke exact-call approvals granted by this Runner's interaction host.
    pub fn clear_approvals(&mut self) {
        self.approvals.clear();
    }

    /// The session. Do not hold the guard across `.await`: the run locks it for every message.
    pub fn session(&self) -> MutexGuard<'_, SessionStore> {
        lock(&self.session)
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

        let sink = PersistingSink {
            session: &self.session,
            inner: self.sink.as_ref(),
            cancel: &run_cancel,
            failure: Mutex::new(None),
            last_assistant_entry: Mutex::new(None),
        };

        // Entry IDs remain stable even when hooks omit messages from earlier runs.
        let run_start = lock(&self.session).entries().len();
        let mut prompts = vec![Message::user(prompt)];
        let mut turns: u32 = 0;
        let mut retries = 0;
        let mut usage = Usage::default();
        let mut last_error = None;
        let mut continuations = 0;

        let ending = loop {
            // The session projection, not an in-memory list, is what the model sees
            // (pi: AgentSession replaces the context with the projection before each request).
            let mut context = lock(&self.session).messages();
            let mut control = LoopControl::new(
                self.limits.max_turns.map(|limit| limit.saturating_sub(turns)),
                &mut self.approvals,
                &run_cancel,
                &sink,
            );
            control.turn_offset = turns;
            control.continuations = continuations;
            let result = run_loop(&mut context, std::mem::take(&mut prompts), &self.config, &mut control).await;
            turns = turns.saturating_add(result.turns);
            continuations = result.continuations;
            usage.add(&result.usage);
            if result.outcome != LoopOutcome::Error {
                break result.outcome;
            }
            // Hook failures are distinct from failed provider responses and are not retried.
            let failed = match result.failure {
                Some(LoopFailure::Provider(message)) => message,
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
            if !retryable || retries >= self.retry.max_retries || run_cancel.is_cancelled() {
                break LoopOutcome::Error;
            }

            retries += 1;
            let delay = self.retry.delay_for(retries, &failed);
            self.config
                .hooks
                .emit(
                    AgentEvent::RetryScheduled {
                        attempt: retries,
                        max_retries: self.retry.max_retries,
                        delay_ms: duration_ms(delay),
                        error: last_error.clone().unwrap_or_default(),
                    },
                    &run_cancel,
                    &sink,
                )
                .await;
            // Keep the failed attempt in the file but drop it from model context, then
            // continue from the previous message (pi: AgentSession._prepareRetry and
            // _omitRecoveryAttempt). No user message is repeated.
            if let Some(entry_id) = sink.take_last_assistant_entry() {
                let omitted = self.session().append_context_edit(&entry_id, None);
                if let Err(e) = omitted {
                    sink.record_failure(format!("cannot omit the failed attempt: {e:#}"));
                    break LoopOutcome::Error;
                }
            }
            let cancelled = tokio::select! {
                _ = tokio::time::sleep(delay) => false,
                _ = run_cancel.cancelled() => true,
            };
            if cancelled {
                break LoopOutcome::Aborted;
            }
        };

        let persistence_failure = lock(&sink.failure).take();
        let (outcome, error) = match (persistence_failure, ending) {
            (Some(failure), _) => (RunOutcome::Error, Some(failure)),
            (None, LoopOutcome::Truncated) => (
                RunOutcome::Truncated,
                Some("the answer was cut off at the output token limit".to_string()),
            ),
            (None, LoopOutcome::Completed) => (RunOutcome::Completed, None),
            (None, LoopOutcome::Error) => (RunOutcome::Error, last_error),
            (None, LoopOutcome::MaxTurns) => (
                RunOutcome::MaxTurns,
                Some(format!(
                    "turn limit reached (max_turns = {})",
                    self.limits
                        .max_turns
                        .expect("MaxTurns requires a configured turn limit")
                )),
            ),
            (None, LoopOutcome::Aborted) if timed_out.load(Ordering::SeqCst) => {
                let secs = self.limits.timeout.map(|t| t.as_secs()).unwrap_or_default();
                (
                    RunOutcome::Timeout,
                    Some(format!("run exceeded the {secs}s time limit")),
                )
            }
            (None, LoopOutcome::Aborted) => (RunOutcome::Aborted, Some("run was cancelled".to_string())),
        };

        let session = self.session();
        // An earlier run's answer must not be reported as this run's.
        let new_ids: HashSet<&str> = session.entries()[run_start..]
            .iter()
            .filter_map(|entry| match entry {
                Entry::Message(message) => Some(message.id.as_str()),
                Entry::ContextEdit(_) => None,
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

/// Persists finished messages before forwarding events (pi: AgentSession writes on
/// `message_end`). If writing fails the run is cancelled: an unattended run must not
/// keep acting without an audit trail.
struct PersistingSink<'a> {
    session: &'a Mutex<SessionStore>,
    inner: &'a dyn EventSink,
    cancel: &'a CancellationToken,
    failure: Mutex<Option<String>>,
    last_assistant_entry: Mutex<Option<String>>,
}

impl PersistingSink<'_> {
    fn take_last_assistant_entry(&self) -> Option<String> {
        lock(&self.last_assistant_entry).take()
    }

    fn record_failure(&self, message: String) {
        lock(&self.failure).get_or_insert(message);
        self.cancel.cancel();
    }
}

impl EventSink for PersistingSink<'_> {
    fn edit_context(&self, index: usize, replacement: Option<&str>) -> Result<(), String> {
        let edited = {
            let mut session = lock(self.session);
            let projection = session.projection();
            match projection.get(index) {
                Some(target) => session
                    .append_context_edit(&target.entry_id, replacement.map(str::to_string))
                    .map(|_| ())
                    .map_err(|error| format!("cannot persist context edit: {error:#}")),
                None => Err(format!("context index {index} is out of bounds")),
            }
        };
        if let Err(error) = &edited {
            self.record_failure(error.clone());
        }
        edited
    }

    fn emit(&self, event: AgentEvent) {
        if let AgentEvent::MessageEnd { message } = &event {
            // The event owns the message and is forwarded below, so the session stores a copy.
            let appended = lock(self.session).append_message(message.clone());
            match appended {
                Ok(id) => {
                    if matches!(message, Message::Assistant(_)) {
                        *lock(&self.last_assistant_entry) = Some(id);
                    }
                }
                Err(e) => self.record_failure(format!("cannot persist session entry: {e:#}")),
            }
        }
        self.inner.emit(event);
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
