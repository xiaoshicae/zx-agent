//! Events emitted while the agent runs, the envelope that places each one in a trace, and the
//! sink that receives them.

use serde::Serialize;
use serde_json::Value;

use super::report::{RunOutcome, RunReport};
use crate::ai::{Delta, Message, Usage};

/// Version of the event JSON, envelope and event types together. Raised on incompatible
/// changes; events without a version predate the envelope.
pub const EVENT_SCHEMA_VERSION: u32 = 3;

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
    /// The ID of the session the run belongs to.
    pub session_id: String,
    /// The run that emitted the event: one prompt, or one manual compaction.
    pub run_id: String,
    #[serde(flatten)]
    pub event: AgentEvent,
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

/// What is known about a tool call that an interrupted run left without a result. The
/// result recorded during recovery tells the model the same thing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Interruption {
    /// The call started and may have changed state; its outcome is unknown.
    Started,
    /// Whether the call started is unknown: the tool is no longer configured, so whether it
    /// has side effects, whose starts are recorded, is unknown too. Treated like `Started`.
    StartUnknown,
    /// A call with side effects that never started, so it had no effect.
    NotStarted,
    /// A read-only call: it changed nothing, whether or not it ran.
    ReadOnly,
}

/// Which extension point of the loop a hook call is at: one per [`Hooks`](super::Hooks)
/// method, and the approval wait. [`HookStage`](super::HookStage) is something else: the
/// order of hooks at the same point. In JSON, the name [`HookPoint::as_str`] gives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookPoint {
    OnInput,
    TransformContext,
    PrepareRequest,
    BeforeTool,
    AfterTool,
    FinishTurn,
    OnEvent,
    Approval,
}

impl HookPoint {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OnInput => "on_input",
            Self::TransformContext => "transform_context",
            Self::PrepareRequest => "prepare_request",
            Self::BeforeTool => "before_tool",
            Self::AfterTool => "after_tool",
            Self::FinishTurn => "finish_turn",
            Self::OnEvent => "on_event",
            Self::Approval => "approval",
        }
    }
}

impl std::fmt::Display for HookPoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for HookPoint {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// Events emitted while the agent runs.
///
/// A run is one prompt: `run_start`, then one attempt per try (`agent_start` to `agent_end`;
/// a retry or a recovery from context overflow starts another), then `run_end`. A turn is one
/// task request plus its tool calls. Summary requests of an automatic compaction belong to
/// the turn they precede but are not turns themselves, although they count against the same
/// request budget; those of an overflow recovery come between two attempts.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
#[expect(
    clippy::large_enum_variant,
    reason = "a message_end carries the whole message; events are built once and handed on, and boxing the message would put a Box in every match on it"
)]
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
    /// Start of one attempt; `attempt` is 1 for the first and grows with each retry or
    /// recovery from context overflow.
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
        /// The `request_started` record in the session.
        entry_id: String,
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
    /// A finished message, already recorded in the session.
    MessageEnd {
        message: Message,
        /// The message's session entry.
        entry_id: String,
        /// Present when the message came from an extension: what its `custom_message`
        /// record holds besides the content the model sees.
        #[serde(skip_serializing_if = "Option::is_none")]
        custom: Option<CustomMessageInfo>,
    },
    /// An extension recorded state in the session (pi: `entry_appended` for a `custom`
    /// entry).
    CustomEntry {
        entry_id: String,
        custom_type: String,
    },
    /// Processing of a tool call begins: validation, policy and approval come before it
    /// runs, so `duration_ms` at the end covers only the execution.
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: Value,
    },
    /// A filtered, transient progress update from a running tool.
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        content: String,
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
        /// The tool's `ToolOutput::details` after the `after_tool` hooks, as recorded with the
        /// result; absent when the tool gave none or they exceeded `MAX_DETAILS_BYTES`.
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        /// What the tool spent on work of its own (`ToolOutput::usage`), as recorded with the
        /// result; absent when the tool reported none.
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    TurnEnd {
        turn: u32,
    },
    /// End of one attempt. The runner may still retry afterwards.
    AgentEnd {
        outcome: RunOutcome,
    },
    /// Emitted by the runner before waiting to retry a failed response. `retry` is 1 for the
    /// first retry, up to `max_retries`; the attempt the retry starts has its own number in
    /// `agent_start`, which recoveries from context overflow raise too.
    RetryScheduled {
        retry: u32,
        max_retries: u32,
        delay_ms: u64,
        error: String,
    },
    /// Emitted by the runner when the model rejected a request as too long for its context
    /// and automatic compaction is enabled. The failed answer is omitted from the context, a
    /// compaction follows (`compaction_start` with `automatic`), and a new attempt continues
    /// from the compacted context. Once per run until an answer succeeds again.
    ContextOverflow {
        error: String,
    },
    /// A recorded change to an existing model-visible message.
    ContextEdited {
        /// The message's position in the context before the change.
        index: usize,
        /// The message's new text; `None` when the message was omitted from the context.
        replacement: Option<String>,
        /// The `context_edit` session entry.
        entry_id: String,
    },
    /// A hook failure. Observer failures produce diagnostics without stopping the run.
    HookFailed {
        hook: String,
        /// Where the hook was called.
        phase: HookPoint,
        error: String,
    },
    /// A compaction's start and its summary request are recorded; the model is asked next.
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
    /// A compaction failed; the preceding projection is retained.
    CompactionFailed {
        error: String,
        automatic: bool,
    },
    /// Before the next request, recovery recorded an error result, reported by the
    /// `message_end` just before this event, for a tool call that an interrupted run left
    /// without one or that a failed or cancelled answer never ran. The call is never
    /// executed again.
    ToolCallRecovered {
        tool_call_id: String,
        tool_name: String,
        state: Interruption,
    },
}

impl AgentEvent {
    /// The serialized `type` name, such as `turn_start`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::RunStart { .. } => "run_start",
            Self::RunEnd { .. } => "run_end",
            Self::AgentStart { .. } => "agent_start",
            Self::TurnStart { .. } => "turn_start",
            Self::RequestStart { .. } => "request_start",
            Self::MessageStart { .. } => "message_start",
            Self::MessageUpdate { .. } => "message_update",
            Self::MessageEnd { .. } => "message_end",
            Self::ToolExecutionStart { .. } => "tool_execution_start",
            Self::ToolExecutionUpdate { .. } => "tool_execution_update",
            Self::ToolDecision { .. } => "tool_decision",
            Self::ApprovalRequested { .. } => "approval_requested",
            Self::ApprovalResolved { .. } => "approval_resolved",
            Self::ToolExecutionEnd { .. } => "tool_execution_end",
            Self::TurnEnd { .. } => "turn_end",
            Self::AgentEnd { .. } => "agent_end",
            Self::RetryScheduled { .. } => "retry_scheduled",
            Self::ContextOverflow { .. } => "context_overflow",
            Self::ContextEdited { .. } => "context_edited",
            Self::HookFailed { .. } => "hook_failed",
            Self::CompactionStart { .. } => "compaction_start",
            Self::CompactionEnd { .. } => "compaction_end",
            Self::CompactionFailed { .. } => "compaction_failed",
            Self::ToolCallRecovered { .. } => "tool_call_recovered",
            Self::CustomEntry { .. } => "custom_entry",
        }
    }
}

/// What a `custom_message` record holds besides the content the model sees; see
/// [`crate::session::CustomMessage`].
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMessageInfo {
    pub custom_type: String,
    pub display: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
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
        self(event);
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
    #[expect(clippy::too_many_lines, reason = "one event of every variant")]
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
                    model_requests: 1,
                    retries: 0,
                    usage: Usage::default(),
                    tool_usage: std::collections::BTreeMap::new(),
                    denied_calls: 0,
                    error: None,
                    session_file: None,
                }),
            },
            AgentEvent::AgentStart { attempt: 1 },
            AgentEvent::TurnStart { turn: 1 },
            AgentEvent::RequestStart {
                kind: RequestKind::Task,
                entry_id: "r".into(),
                model: "m".into(),
                attempt: 1,
            },
            AgentEvent::MessageStart { role: "user" },
            AgentEvent::MessageUpdate {
                delta: Delta::Text { text: "x".into() },
            },
            AgentEvent::MessageEnd {
                message: Message::user("x"),
                entry_id: "e".into(),
                custom: None,
            },
            AgentEvent::CustomEntry {
                entry_id: "c".into(),
                custom_type: "state".into(),
            },
            AgentEvent::ToolExecutionStart {
                tool_call_id: "c".into(),
                tool_name: "bash".into(),
                args: json!({}),
            },
            AgentEvent::ToolExecutionUpdate {
                tool_call_id: "c".into(),
                tool_name: "bash".into(),
                content: "half done".into(),
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
                details: None,
                usage: None,
            },
            AgentEvent::TurnEnd { turn: 1 },
            AgentEvent::AgentEnd {
                outcome: RunOutcome::Completed,
            },
            AgentEvent::RetryScheduled {
                retry: 1,
                max_retries: 3,
                delay_ms: 10,
                error: "e".into(),
            },
            AgentEvent::ContextOverflow {
                error: "too long".into(),
            },
            AgentEvent::ContextEdited {
                index: 0,
                replacement: None,
                entry_id: "x".into(),
            },
            AgentEvent::HookFailed {
                hook: "test".into(),
                phase: HookPoint::OnEvent,
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
        for event in &events {
            assert_eq!(serde_json::to_value(event).unwrap()["type"], event.kind());
        }
        let mut kinds: Vec<&str> = events.iter().map(AgentEvent::kind).collect();
        kinds.sort_unstable();
        kinds.dedup();
        assert_eq!(kinds.len(), 25, "every variant was checked: {kinds:?}");
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
    fn a_hook_failure_names_the_hook_method_as_its_phase() {
        let event = AgentEvent::HookFailed {
            hook: "audit".into(),
            phase: HookPoint::AfterTool,
            error: "audit after_tool failed: full".into(),
        };
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            json!({"type": "hook_failed", "hook": "audit", "phase": "after_tool", "error": "audit after_tool failed: full"})
        );
    }

    #[test]
    fn event_fields_are_camel_case() {
        // Consumers of `--output jsonl` read these names.
        let event = AgentEvent::ToolExecutionEnd {
            tool_call_id: "c".into(),
            tool_name: "bash".into(),
            is_error: true,
            duration_ms: Some(5),
            details: None,
            usage: None,
        };
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            json!({"type": "tool_execution_end", "toolCallId": "c", "toolName": "bash", "isError": true, "durationMs": 5})
        );
    }
}
