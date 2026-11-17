//! Resolve tool calls left unfinished by an interrupted run.

use anyhow::{Context, Result};

use super::run_scope::RunScope;
use super::{PERSIST_FAILED, Runner};
use crate::ai::{Message, ToolResultMessage, now_ms};
use crate::events::{EventKind, Interruption};
use crate::tools::{Effects, Toolbox};
use crate::transcript::{StartRecord, Transcript};

impl RunScope<'_> {
    /// Record an error result for each call that an interrupted run left without one, so
    /// the next request cannot suggest that such a call did nothing. Calls are never run
    /// again. Stops at the first result the session cannot record.
    pub(super) async fn recover_interrupted_calls(&mut self) -> Result<()> {
        for (result, recovered) in recovered_results(self.transcript, &self.config.tools) {
            self.target.emit(EventKind::MessageStart { role: "toolResult" }).await;
            let entry_id = self
                .transcript
                .append_message(result.clone())
                .await
                .context(PERSIST_FAILED)?;
            self.target
                .emit(EventKind::MessageEnd {
                    message: result,
                    entry_id,
                    custom: None,
                })
                .await;
            // Reported once recorded, so the event never claims a result the session lacks.
            self.target.emit(recovered).await;
        }
        Ok(())
    }
}

impl Runner {
    /// Record the results a run's start records, while idle and without events, as no run
    /// reads them: before a record the model reads, which would otherwise come between the
    /// calls and their results and leave the calls unanswered for good.
    pub(crate) async fn recover_interrupted_calls(&mut self) -> Result<()> {
        for (result, _) in recovered_results(&self.transcript, &self.config.tools) {
            self.transcript.append_message(result).await.context(PERSIST_FAILED)?;
        }
        Ok(())
    }
}

/// The result to record for each call that an interrupted run left without one, in call
/// order, with the event that reports it.
fn recovered_results(transcript: &Transcript, tools: &Toolbox) -> Vec<(Message, EventKind)> {
    transcript
        .interrupted_tool_calls()
        .into_iter()
        .map(|call| {
            let state = interruption(call.start, tools.effects(&call.tool_name));
            let recovered = EventKind::ToolCallRecovered {
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
                usage: None,
            });
            (result, recovered)
        })
        .collect()
}

/// What recovery may claim about an interrupted call. A start record means the call may
/// have taken effect; its absence proves nothing for read-only tools, which are never
/// recorded, or for tools that are no longer configured, whose effects are unknown.
fn interruption(start: StartRecord, effects: Option<Effects>) -> Interruption {
    match (start, effects) {
        (StartRecord::Present, _) => Interruption::Started,
        (StartRecord::NeverRun, _) | (StartRecord::Absent, Some(Effects::SideEffects)) => Interruption::NotStarted,
        (StartRecord::Absent, Some(Effects::ReadOnly)) => Interruption::ReadOnly,
        (StartRecord::Absent, None) => Interruption::StartUnknown,
    }
}

/// The result the model sees for an interrupted call.
fn recovered_result(state: Interruption) -> &'static str {
    match state {
        Interruption::Started => {
            "Result unknown: this call started, but its result is not recorded here: the agent stopped before recording it, or the conversation went back to before it. It may have changed state; check the current state before running it again."
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
