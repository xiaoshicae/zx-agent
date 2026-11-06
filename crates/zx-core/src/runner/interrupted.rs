//! Resolve tool calls left unfinished by an interrupted run.

use super::Runner;
use crate::agent::{AgentEvent, Effects, EventTarget, Interruption, RunTrace};
use crate::ai::{Message, ToolResultMessage, now_ms};
use crate::session::StartRecord;

impl Runner {
    /// Record an error result for each call that an interrupted run left without one, so
    /// the next request cannot suggest that such a call did nothing. Calls are never run
    /// again. Stops at the first result the session cannot record.
    pub(super) async fn recover_interrupted_calls(&mut self, trace: &RunTrace) -> Result<(), String> {
        let Self {
            config, session, sink, ..
        } = self;
        let to = EventTarget {
            sink: sink.as_ref(),
            trace,
        };
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
            let result = Message::ToolResult(ToolResultMessage {
                tool_call_id: call.tool_call_id,
                tool_name: call.tool_name,
                content: recovered_result(state).to_string(),
                is_error: true,
                timestamp: now_ms(),
                duration_ms: None,
                details: None,
            });
            let start = AgentEvent::MessageStart { role: "toolResult" };
            config.hooks.emit(start, to).await;
            let entry_id = session
                .append_message(result.clone())
                .await
                .map_err(|error| format!("cannot persist session entry: {error:#}"))?;
            let end = AgentEvent::MessageEnd {
                message: result,
                entry_id: Some(entry_id),
                custom: None,
            };
            config.hooks.emit(end, to).await;
            // Reported once recorded, so the event never claims a result the session lacks.
            config.hooks.emit(event, to).await;
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
        (StartRecord::NeverRun, _) | (StartRecord::Absent, Some(Effects::SideEffects)) => Interruption::NotStarted,
        (StartRecord::Absent | StartRecord::Unavailable, Some(Effects::ReadOnly)) => Interruption::ReadOnly,
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
