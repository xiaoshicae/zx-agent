//! Events emitted while the agent runs, and the sink that receives them.

use serde::Serialize;
use serde_json::Value;

use crate::ai::{Delta, Message};

/// How one `run_loop` call ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopOutcome {
    /// The model answered without further tool calls and no queued message remained.
    Completed,
    /// The provider returned an error response.
    Error,
    /// Cancelled.
    Aborted,
    /// The turn budget ran out while the model still wanted to continue.
    MaxTurns,
}

/// Events emitted while the agent runs (pi: `AgentEvent`, packages/agent/src/types.ts).
///
/// A turn is one model response plus the tool calls it requested.
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
        }
    }
}

/// Receives events. Called synchronously and possibly from concurrently running tool
/// futures, so implementations use interior mutability and must not block for long.
pub trait EventSink: Send + Sync {
    /// Handle one event.
    fn emit(&self, event: AgentEvent);
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
        ];
        for event in events {
            assert_eq!(serde_json::to_value(&event).unwrap()["type"], event.kind());
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
