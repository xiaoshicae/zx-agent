//! Events emitted while the agent runs, and the sink that receives them.

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::ai::{Delta, Message, Provider, Request, ToolCall, Usage};

/// How one `run_loop` call ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopOutcome {
    /// The model answered without further tool calls and no queued message remained.
    Completed,
    /// A provider or operational hook failed, or a context edit could not be persisted.
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
/// A turn is one model request plus its tool calls. Automatic host preparation requests
/// count against the same turn budget; manual compaction reports its own costs.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum AgentEvent {
    AgentStart,
    TurnStart {
        turn: u32,
    },
    MessageStart {
        role: &'static str,
    },
    /// Streaming output of the assistant message in progress.
    MessageUpdate {
        delta: Delta,
    },
    /// A finished message. The runner persists messages at this point.
    MessageEnd {
        message: Message,
    },
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: Value,
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
    /// End of one loop run. The runner may still retry afterwards.
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
    /// A persisted change to an existing model-visible message.
    ContextEdited {
        index: usize,
        replacement: Option<String>,
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
            AgentEvent::AgentStart => "agent_start",
            AgentEvent::TurnStart { .. } => "turn_start",
            AgentEvent::MessageStart { .. } => "message_start",
            AgentEvent::MessageUpdate { .. } => "message_update",
            AgentEvent::MessageEnd { .. } => "message_end",
            AgentEvent::ToolExecutionStart { .. } => "tool_execution_start",
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

/// Receives events and persists edits synchronously, possibly from concurrent tool
/// futures. These methods must not block for long. prepare_context is the asynchronous
/// session-host boundary before the next task request.
#[async_trait]
pub trait EventSink: Send + Sync {
    /// Handle one event.
    fn emit(&self, event: AgentEvent);
    /// Persist an edit before the loop changes its projection. Memory-only sinks
    /// need no audit writer; the Runner supplies a session-backed implementation.
    fn edit_context(&self, _index: usize, _replacement: Option<&str>) -> Result<(), String> {
        Ok(())
    }
    /// Persist a request checkpoint before dispatch. The default is memory-only;
    /// a Runner records configuration and projection in its append-only session.
    fn record_request(&self, _request: &Request, _provider: &dyn Provider) -> Result<(), String> {
        Ok(())
    }
    /// Persist that a call with side effects is about to run, after its checks and approval.
    /// An error prevents the call. The default is memory-only; a Runner appends
    /// `tool_call_started` to its session.
    fn record_tool_call_started(&self, _call: &ToolCall) -> Result<(), String> {
        Ok(())
    }
    /// Session-host preparation after input/context edits and before a task request.
    /// The default makes no request or change. A replacement must already be persisted;
    /// request costs, including failed preparation, count against the run budget.
    async fn prepare_context(&self, _cancel: &CancellationToken) -> ContextPreparation {
        ContextPreparation::Unchanged
    }
}

/// A host-owned preparation step. The loop knows neither profiles nor compaction policy.
pub enum ContextPreparation {
    Unchanged,
    Updated {
        messages: Vec<Message>,
        requests: u32,
        usage: Usage,
    },
    Failed {
        error: String,
        requests: u32,
        usage: Usage,
    },
}

impl<F> EventSink for F
where
    F: Fn(AgentEvent) + Send + Sync,
{
    fn emit(&self, event: AgentEvent) {
        self(event)
    }
}

/// Sink that discards every event.
pub struct NullSink;

impl EventSink for NullSink {
    fn emit(&self, _event: AgentEvent) {}
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn kind_matches_the_serialized_type() {
        // One event of every variant; add new variants here.
        let events = [
            AgentEvent::AgentStart,
            AgentEvent::TurnStart { turn: 1 },
            AgentEvent::MessageStart { role: "user" },
            AgentEvent::MessageUpdate {
                delta: Delta::Text { text: "x".into() },
            },
            AgentEvent::MessageEnd {
                message: Message::user("x"),
            },
            AgentEvent::ToolExecutionStart {
                tool_call_id: "c".into(),
                tool_name: "bash".into(),
                args: json!({}),
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
}
