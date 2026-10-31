//! Run-level control around the loop (a small subset of pi's coding-agent AgentSession).
//!
//! The loop stops at the first provider error; this layer decides whether to retry, keeps
//! the session file as the source of model context, and enforces turn and time limits.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::agent::{AgentEvent, EventSink, LoopConfig, LoopOutcome, run_loop};
use crate::ai::retry::{RetryPolicy, is_retryable};
use crate::ai::{AssistantMessage, Message, StopReason, Usage, duration_ms};
use crate::session::SessionStore;
use crate::sync::{AbortOnDrop, lock};

/// Bounds for one run. Unattended runs have no one to stop them, so both default to finite values.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Model requests per run, failed attempts included.
    pub max_turns: u32,
    /// Wall-clock limit for the whole run; `None` disables it.
    pub timeout: Option<Duration>,
    /// How many times an answer cut off at the output token limit is continued by asking
    /// the model to go on. Each continuation is a model request and counts as a turn.
    /// Interactive callers usually set 0 and leave it to the user, as pi does.
    pub continue_on_length: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_turns: 30,
            timeout: Some(Duration::from_secs(15 * 60)),
            continue_on_length: 1,
        }
    }
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
    /// A request failed and retrying did not help, or the session file could not be written.
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
        }
    }

    /// The loop configuration, for example to reach `config().queues` during a run.
    pub fn config(&self) -> &LoopConfig {
        &self.config
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

        // Messages before this index belong to earlier runs. Context edits only ever omit this
        // run's failed attempts, so the earlier part of the projection does not change.
        let run_start = self.session().messages().len();
        let mut prompts = vec![Message::user(prompt)];
        let mut turns = 0;
        let mut retries = 0;
        let mut usage = Usage::default();
        let mut last_error = None;
        let mut continuations = 0;

        let ending = loop {
            // The session projection, not an in-memory list, is what the model sees
            // (pi: AgentSession replaces the context with the projection before each request).
            let mut context = self.session().messages();
            let first_new = context.len();
            let result = run_loop(
                &mut context,
                std::mem::take(&mut prompts),
                &self.config,
                self.limits.max_turns.saturating_sub(turns),
                &run_cancel,
                &sink,
            )
            .await;
            let new_messages = &context[first_new..];
            turns += result.turns;
            for message in new_messages {
                if let Message::Assistant(assistant) = message {
                    usage.add(&assistant.usage);
                }
            }
            match result.outcome {
                LoopOutcome::Error => {}
                LoopOutcome::Completed => {
                    let truncated = last_assistant(new_messages).is_some_and(|m| m.stop_reason == StopReason::Length);
                    if !truncated {
                        break Ending::Loop(LoopOutcome::Completed);
                    }
                    let budget_left = turns < self.limits.max_turns && !run_cancel.is_cancelled();
                    if continuations >= self.limits.continue_on_length || !budget_left {
                        break Ending::Truncated;
                    }
                    continuations += 1;
                    prompts.push(Message::user(CONTINUE_PROMPT));
                    continue;
                }
                other => break Ending::Loop(other),
            }

            let failed = last_assistant(new_messages);
            last_error = failed.and_then(|message| message.error_message.clone());
            let retryable = failed.is_some_and(is_retryable);
            if !retryable || retries >= self.retry.max_retries || run_cancel.is_cancelled() {
                break Ending::Loop(LoopOutcome::Error);
            }

            retries += 1;
            // A retryable error always has a failed assistant message.
            let delay = self
                .retry
                .delay_for(retries, failed.expect("a retryable failure has an assistant message"));
            self.sink.emit(AgentEvent::RetryScheduled {
                attempt: retries,
                max_retries: self.retry.max_retries,
                delay_ms: duration_ms(delay),
                error: last_error.clone().unwrap_or_default(),
            });
            // Keep the failed attempt in the file but drop it from model context, then
            // continue from the previous message (pi: AgentSession._prepareRetry and
            // _omitRecoveryAttempt). No user message is repeated.
            if let Some(entry_id) = sink.take_last_assistant_entry() {
                let omitted = self.session().append_context_edit(&entry_id, None);
                if let Err(e) = omitted {
                    sink.record_failure(format!("cannot omit the failed attempt: {e:#}"));
                    break Ending::Loop(LoopOutcome::Error);
                }
            }
            let cancelled = tokio::select! {
                _ = tokio::time::sleep(delay) => false,
                _ = run_cancel.cancelled() => true,
            };
            if cancelled {
                break Ending::Loop(LoopOutcome::Aborted);
            }
        };

        let persistence_failure = lock(&sink.failure).take();
        let (outcome, error) = match (persistence_failure, ending) {
            (Some(failure), _) => (RunOutcome::Error, Some(failure)),
            (None, Ending::Truncated) => (
                RunOutcome::Truncated,
                Some("the answer was cut off at the output token limit".to_string()),
            ),
            (None, Ending::Loop(LoopOutcome::Completed)) => (RunOutcome::Completed, None),
            (None, Ending::Loop(LoopOutcome::Error)) => (RunOutcome::Error, last_error),
            (None, Ending::Loop(LoopOutcome::MaxTurns)) => (
                RunOutcome::MaxTurns,
                Some(format!("turn limit reached (max_turns = {})", self.limits.max_turns)),
            ),
            (None, Ending::Loop(LoopOutcome::Aborted)) if timed_out.load(Ordering::SeqCst) => {
                let secs = self.limits.timeout.map(|t| t.as_secs()).unwrap_or_default();
                (
                    RunOutcome::Timeout,
                    Some(format!("run exceeded the {secs}s time limit")),
                )
            }
            (None, Ending::Loop(LoopOutcome::Aborted)) => (RunOutcome::Aborted, Some("run was cancelled".to_string())),
        };

        let session = self.session();
        // An earlier run's answer must not be reported as this run's.
        let messages = session.messages();
        let final_text = final_answer(messages.get(run_start..).unwrap_or_default());
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

/// Sent when an answer is cut off at the output token limit.
const CONTINUE_PROMPT: &str = "Your previous response was cut off at the output token limit. Continue exactly where it \
stopped, without repeating earlier text, and keep the rest concise.";

/// How the retry loop ended, before persistence failures and timeouts are taken into account.
enum Ending {
    Loop(LoopOutcome),
    /// The answer stayed cut off at the output token limit.
    Truncated,
}

/// The text of the last assistant message, preceded by the parts it continues: an answer
/// continues the previous one when a cut-off response and a continuation prompt precede it.
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
        match &messages[..index] {
            [.., Message::Assistant(previous), Message::User { content, .. }]
                if previous.stop_reason == StopReason::Length && content == CONTINUE_PROMPT =>
            {
                end = index - 1;
            }
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

fn last_assistant(messages: &[Message]) -> Option<&AssistantMessage> {
    messages.iter().rev().find_map(|message| match message {
        Message::Assistant(assistant) => Some(assistant),
        Message::System(_) | Message::User { .. } | Message::ToolResult(_) => None,
    })
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
    use crate::agent::{AllowAll, MessageQueues, NullSink};
    use crate::ai::SystemPrompt;
    use crate::ai::faux::{FauxProvider, FauxStep};
    use crate::session::{FailingWriter, SessionHeader};

    #[tokio::test]
    async fn a_session_write_failure_cancels_the_run() {
        let provider = Arc::new(FauxProvider::new([FauxStep::text("never sent")]));
        let config = LoopConfig {
            provider: provider.clone(),
            model: "m".into(),
            system_prompt: SystemPrompt::default(),
            tools: vec![],
            policy: Arc::new(AllowAll),
            cwd: PathBuf::from("."),
            max_tokens: None,
            sequential_tools: false,
            queues: Arc::new(MessageQueues::default()),
        };
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

    #[test]
    fn overlaps_are_measured_in_bytes_on_char_boundaries() {
        // "端口错误" is 12 bytes, so it counts even though it is only 4 characters.
        assert_eq!(overlap("数据库端口错误", "端口错误: 5433"), "端口错误".len());
    }
}
