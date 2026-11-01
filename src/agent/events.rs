//! Events emitted while the agent runs, the envelope that places each one in a trace, and the
//! sink that receives them.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::hooks::HookContext;
use super::report::RunReport;
use crate::ai::{Delta, Message, Usage, now_ms};

/// Version of the event JSON, envelope and event types together. Raised on incompatible
/// changes; events without a version predate the envelope.
pub const EVENT_SCHEMA_VERSION: u32 = 1;

/// One event, with what a trace needs to place it. In JSON the envelope fields and the
/// event's own fields share one object, and `type` names the event.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    /// [`EVENT_SCHEMA_VERSION`] when the event was emitted.
    pub schema_version: u32,
    /// Position within the run, from 1. A gap means that the subscriber did not receive some
    /// events: ones a bounded subscription dropped, or ones it filtered out, such as the
    /// streaming deltas that `--output jsonl` leaves out without `--deltas`.
    pub seq: u64,
    /// When the event was emitted, in milliseconds since the Unix epoch.
    pub at_ms: u64,
    /// The session's ID; absent when `run_loop` runs without a session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The run that emitted the event: one prompt, or one manual compaction.
    pub run_id: String,
    #[serde(flatten)]
    pub event: AgentEvent,
}

/// One run's context: what its hooks are told and what stamps its events, with the next
/// sequence number.
pub(crate) struct RunTrace {
    context: HookContext,
    seq: AtomicU64,
}

impl RunTrace {
    /// A new run with a fresh ID, cancelled by `cancel`.
    pub(crate) fn new(session_id: Option<String>, cwd: PathBuf, cancel: CancellationToken) -> Self {
        Self {
            context: HookContext::new(session_id, cwd, cancel),
            seq: AtomicU64::new(0),
        }
    }

    /// What every hook of this run is told.
    pub(crate) fn context(&self) -> &HookContext {
        &self.context
    }

    /// The event with the next sequence number. Events are stamped in the order they are
    /// emitted, which is also the order their sink receives them.
    pub(crate) fn stamp(&self, event: AgentEvent) -> Event {
        Event {
            schema_version: EVENT_SCHEMA_VERSION,
            seq: self.seq.fetch_add(1, Ordering::Relaxed) + 1,
            at_ms: now_ms(),
            session_id: self.context.session_id.clone(),
            run_id: self.context.run_id.clone(),
            event,
        }
    }
}

/// Where the events of a run go: stamped by `trace`, then handed to `sink`.
#[derive(Clone, Copy)]
pub(crate) struct Emit<'a> {
    pub(crate) sink: &'a dyn EventSink,
    pub(crate) trace: &'a RunTrace,
}

impl Emit<'_> {
    /// Stamp `event` and hand it to the sink only, bypassing `on_event` hooks.
    pub(crate) fn direct(self, event: AgentEvent) {
        self.sink.emit(&self.trace.stamp(event));
    }
}

/// Which model request a `request_start` announces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    /// A request for the task: the conversation so far.
    Task,
    /// A summary request of a compaction.
    Summary,
}

/// What the tool policy concluded about a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolVerdict {
    Allow,
    Deny,
    /// Run only after an approval.
    Ask,
}

/// How an approval ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalResult {
    /// Allowed for this call.
    AllowOnce,
    /// Allowed for this call and identical later calls in the session.
    AllowSession,
    /// An earlier `AllowSession` covered this exact call, so nobody was asked.
    Remembered,
    Denied,
    /// Nobody answered in time; the call does not run.
    TimedOut,
    /// The run was cancelled while waiting; the call does not run.
    Cancelled,
    /// The interaction host failed or panicked; the call does not run.
    Failed,
}

/// How one `run_loop` call ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopOutcome {
    /// The model answered without further tool calls and no queued message remained.
    Completed,
    /// A provider or operational hook failed, or the session could not record.
    Error,
    /// Cancelled.
    Aborted,
    /// The turn budget ran out while the model still wanted to continue.
    MaxTurns,
    /// The answer was cut off and no hook requested a continuation.
    Truncated,
}

/// What is known about a tool call that an interrupted run left without a result. The
/// result recorded during recovery tells the model the same thing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Interruption {
    /// The call started and may have changed state; its outcome is unknown.
    Started,
    /// Whether the call started was not recorded: the session predates start records, or
    /// the tool is no longer configured. Treated like `Started`.
    StartUnknown,
    /// A call with side effects that never started, so it had no effect.
    NotStarted,
    /// A read-only call: it changed nothing, whether or not it ran.
    ReadOnly,
}

impl Interruption {
    /// A short description for progress output.
    pub fn describe(self) -> &'static str {
        match self {
            Interruption::Started => "started, result unknown",
            Interruption::StartUnknown => "start not recorded, result unknown",
            Interruption::NotStarted => "not started",
            Interruption::ReadOnly => "read-only, no result",
        }
    }
}

/// Events emitted while the agent runs (pi: `AgentEvent`, packages/agent/src/types.ts).
///
/// A run is one prompt: `run_start`, then one attempt per try (`agent_start` to `agent_end`;
/// a retry starts another), then `run_end`. A turn is one task request plus its tool calls.
/// Summary requests of an automatic compaction belong to the turn they precede but are not
/// turns themselves, although they count against the same request budget.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum AgentEvent {
    /// The first event of a run.
    RunStart {
        model: String,
        provider: String,
    },
    /// The last event of a run, with what [`Runner::run`](crate::runner::Runner::run)
    /// returns.
    RunEnd {
        report: Box<RunReport>,
    },
    /// Start of one attempt; `attempt` is 1 for the first and grows with each retry.
    AgentStart {
        attempt: u32,
    },
    TurnStart {
        turn: u32,
    },
    /// A model request is about to be sent. Requests that fail before any response, such
    /// as on an HTTP error, start here too.
    RequestStart {
        kind: RequestKind,
        /// The `request_started` record in the session; absent without a session.
        #[serde(skip_serializing_if = "Option::is_none")]
        entry_id: Option<String>,
        model: String,
        attempt: u32,
    },
    MessageStart {
        role: &'static str,
    },
    /// Streaming output of the assistant message in progress.
    MessageUpdate {
        delta: Delta,
    },
    /// A finished message, already recorded when the run has a session.
    MessageEnd {
        message: Message,
        /// The message's session entry; absent without a session.
        #[serde(skip_serializing_if = "Option::is_none")]
        entry_id: Option<String>,
    },
    /// Processing of a tool call begins: validation, policy and approval come before it
    /// runs, so `duration_ms` at the end covers only the execution.
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: Value,
    },
    /// The tool policy's verdict on a call. A policy hook that fails denies the call.
    ToolDecision {
        tool_call_id: String,
        tool_name: String,
        verdict: ToolVerdict,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// The interaction host is asked to approve a call.
    ApprovalRequested {
        tool_call_id: String,
        tool_name: String,
        reason: String,
    },
    /// An approval ended. `remembered` follows no request: nobody was asked.
    ApprovalResolved {
        tool_call_id: String,
        tool_name: String,
        approval: ApprovalResult,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        wait_ms: u64,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    TurnEnd {
        turn: u32,
    },
    /// End of one attempt. The runner may still retry afterwards.
    AgentEnd {
        outcome: LoopOutcome,
    },
    /// Emitted by the runner before waiting to retry a failed response.
    RetryScheduled {
        attempt: u32,
        max_retries: u32,
        delay_ms: u64,
        error: String,
    },
    /// A recorded change to an existing model-visible message.
    ContextEdited {
        index: usize,
        replacement: Option<String>,
        /// The `context_edit` session entry; absent without a session.
        #[serde(skip_serializing_if = "Option::is_none")]
        entry_id: Option<String>,
    },
    /// A hook failure. Observer failures produce diagnostics without stopping the run.
    HookFailed {
        hook: String,
        phase: &'static str,
        error: String,
    },
    /// The host persisted a summary request before calling the model.
    CompactionStart {
        input_bytes: usize,
        automatic: bool,
    },
    /// The summary was validated and persisted before changing model context.
    CompactionEnd {
        entry_id: String,
        input_bytes: usize,
        output_bytes: usize,
        usage: Usage,
        automatic: bool,
    },
    /// Before the next request, recovery recorded an error result for a tool call that an
    /// interrupted run left without one. The call is never executed again.
    ToolCallRecovered {
        tool_call_id: String,
        tool_name: String,
        state: Interruption,
    },
    /// A summary failed; the preceding projection is retained.
    CompactionFailed {
        error: String,
        automatic: bool,
    },
}

impl AgentEvent {
    /// The serialized `type` name, such as `turn_start`.
    pub fn kind(&self) -> &'static str {
        match self {
            AgentEvent::RunStart { .. } => "run_start",
            AgentEvent::RunEnd { .. } => "run_end",
            AgentEvent::AgentStart { .. } => "agent_start",
            AgentEvent::TurnStart { .. } => "turn_start",
            AgentEvent::RequestStart { .. } => "request_start",
            AgentEvent::MessageStart { .. } => "message_start",
            AgentEvent::MessageUpdate { .. } => "message_update",
            AgentEvent::MessageEnd { .. } => "message_end",
            AgentEvent::ToolExecutionStart { .. } => "tool_execution_start",
            AgentEvent::ToolDecision { .. } => "tool_decision",
            AgentEvent::ApprovalRequested { .. } => "approval_requested",
            AgentEvent::ApprovalResolved { .. } => "approval_resolved",
            AgentEvent::ToolExecutionEnd { .. } => "tool_execution_end",
            AgentEvent::TurnEnd { .. } => "turn_end",
            AgentEvent::AgentEnd { .. } => "agent_end",
            AgentEvent::RetryScheduled { .. } => "retry_scheduled",
            AgentEvent::ContextEdited { .. } => "context_edited",
            AgentEvent::HookFailed { .. } => "hook_failed",
            AgentEvent::CompactionStart { .. } => "compaction_start",
            AgentEvent::CompactionEnd { .. } => "compaction_end",
            AgentEvent::CompactionFailed { .. } => "compaction_failed",
            AgentEvent::ToolCallRecovered { .. } => "tool_call_recovered",
        }
    }
}

/// Receives events. Called from the running loop, possibly from concurrent tool futures,
/// so it must return promptly. Events report what happened; the session records it first.
pub trait EventSink: Send + Sync {
    /// Handle one event. A sink that keeps events clones what it keeps.
    fn emit(&self, event: &Event);
}

impl<F> EventSink for F
where
    F: Fn(&Event) + Send + Sync,
{
    fn emit(&self, event: &Event) {
        self(event)
    }
}

/// Sink that discards every event.
pub struct NullSink;

impl EventSink for NullSink {
    fn emit(&self, _event: &Event) {}
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::agent::RunOutcome;

    #[test]
    fn kind_matches_the_serialized_type() {
        // One event of every variant; add new variants here.
        let events = [
            AgentEvent::RunStart {
                model: "m".into(),
                provider: "p".into(),
            },
            AgentEvent::RunEnd {
                report: Box::new(RunReport {
                    outcome: RunOutcome::Completed,
                    final_text: "done".into(),
                    turns: 1,
                    retries: 0,
                    usage: Usage::default(),
                    error: None,
                    session_file: None,
                }),
            },
            AgentEvent::AgentStart { attempt: 1 },
            AgentEvent::TurnStart { turn: 1 },
            AgentEvent::RequestStart {
                kind: RequestKind::Task,
                entry_id: None,
                model: "m".into(),
                attempt: 1,
            },
            AgentEvent::MessageStart { role: "user" },
            AgentEvent::MessageUpdate {
                delta: Delta::Text { text: "x".into() },
            },
            AgentEvent::MessageEnd {
                message: Message::user("x"),
                entry_id: Some("e".into()),
            },
            AgentEvent::ToolExecutionStart {
                tool_call_id: "c".into(),
                tool_name: "bash".into(),
                args: json!({}),
            },
            AgentEvent::ToolDecision {
                tool_call_id: "c".into(),
                tool_name: "bash".into(),
                verdict: ToolVerdict::Ask,
                reason: Some("writes files".into()),
            },
            AgentEvent::ApprovalRequested {
                tool_call_id: "c".into(),
                tool_name: "bash".into(),
                reason: "writes files".into(),
            },
            AgentEvent::ApprovalResolved {
                tool_call_id: "c".into(),
                tool_name: "bash".into(),
                approval: ApprovalResult::AllowOnce,
                reason: None,
                wait_ms: 5,
            },
            AgentEvent::ToolExecutionEnd {
                tool_call_id: "c".into(),
                tool_name: "bash".into(),
                is_error: false,
                duration_ms: None,
            },
            AgentEvent::TurnEnd { turn: 1 },
            AgentEvent::AgentEnd {
                outcome: LoopOutcome::Completed,
            },
            AgentEvent::RetryScheduled {
                attempt: 1,
                max_retries: 3,
                delay_ms: 10,
                error: "e".into(),
            },
            AgentEvent::ContextEdited {
                index: 0,
                replacement: None,
                entry_id: None,
            },
            AgentEvent::HookFailed {
                hook: "test".into(),
                phase: "on_event",
                error: "failed".into(),
            },
            AgentEvent::CompactionStart {
                input_bytes: 1000,
                automatic: true,
            },
            AgentEvent::CompactionEnd {
                entry_id: "s".into(),
                input_bytes: 1000,
                output_bytes: 500,
                usage: Usage::default(),
                automatic: true,
            },
            AgentEvent::CompactionFailed {
                error: "failed".into(),
                automatic: false,
            },
            AgentEvent::ToolCallRecovered {
                tool_call_id: "c".into(),
                tool_name: "bash".into(),
                state: Interruption::Started,
            },
        ];
        for event in events {
            assert_eq!(serde_json::to_value(&event).unwrap()["type"], event.kind());
        }
    }

    #[test]
    fn recovered_calls_serialize_their_documented_states() {
        let states = [
            (Interruption::Started, "started"),
            (Interruption::StartUnknown, "start_unknown"),
            (Interruption::NotStarted, "not_started"),
            (Interruption::ReadOnly, "read_only"),
        ];
        for (state, name) in states {
            let event = AgentEvent::ToolCallRecovered {
                tool_call_id: "c".into(),
                tool_name: "bash".into(),
                state,
            };
            assert_eq!(
                serde_json::to_value(&event).unwrap(),
                json!({"type": "tool_call_recovered", "toolCallId": "c", "toolName": "bash", "state": name})
            );
        }
    }

    #[test]
    fn event_fields_are_camel_case() {
        // Consumers of `--output jsonl` read these names.
        let event = AgentEvent::ToolExecutionEnd {
            tool_call_id: "c".into(),
            tool_name: "bash".into(),
            is_error: true,
            duration_ms: Some(5),
        };
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            json!({"type": "tool_execution_end", "toolCallId": "c", "toolName": "bash", "isError": true, "durationMs": 5})
        );
    }

    #[test]
    fn the_envelope_shares_one_object_with_the_event() {
        let trace = RunTrace::new(Some("session".into()), PathBuf::from("."), CancellationToken::new());
        let first = trace.stamp(AgentEvent::AgentStart { attempt: 1 });
        let second = trace.stamp(AgentEvent::TurnStart { turn: 1 });

        let value = serde_json::to_value(&second).unwrap();

        assert_eq!(value["schemaVersion"], EVENT_SCHEMA_VERSION);
        assert_eq!((first.seq, value["seq"].clone()), (1, json!(2)));
        assert_eq!(value["sessionId"], "session");
        assert_eq!(value["runId"], first.run_id.as_str());
        assert_eq!(
            (value["type"].clone(), value["turn"].clone()),
            (json!("turn_start"), json!(1))
        );
        assert!(value["atMs"].as_u64().is_some_and(|ms| ms >= first.at_ms));
        assert!(value.get("event").is_none(), "the event is not nested");
    }

    #[test]
    fn each_run_has_its_own_id_and_numbering() {
        let first = RunTrace::new(None, PathBuf::from("."), CancellationToken::new());
        let second = RunTrace::new(None, PathBuf::from("."), CancellationToken::new());
        first.stamp(AgentEvent::AgentStart { attempt: 1 });

        let event = second.stamp(AgentEvent::AgentStart { attempt: 1 });

        assert_eq!(event.seq, 1);
        assert_ne!(event.run_id, first.stamp(AgentEvent::AgentStart { attempt: 1 }).run_id);
        assert!(serde_json::to_value(&event).unwrap().get("sessionId").is_none());
    }
}
