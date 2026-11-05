//! Model request recording and streaming.

use super::{
    AgentEvent, AssistantMessage, Engine, Halt, LoopResult, Message, Request, RequestKind, StopReason, StreamEvent,
    StreamExt,
};
use crate::agent::RequestSettings;

impl Engine<'_, '_, '_> {
    /// Record and send a task request, then account for its response.
    pub(super) async fn request_model(
        &mut self,
        context: &[Message],
        result: &mut LoopResult,
    ) -> Result<Option<AssistantMessage>, Halt> {
        let spec = &self.config.spec;
        let configured = RequestSettings {
            model: spec.model.clone(),
            reasoning: spec.reasoning,
        };
        let turn = self.turn.unwrap_or_default();
        let settings = self
            .config
            .hooks
            .prepare_request(context, turn, configured, self.trace.context())
            .await
            .map_err(|error| Halt::Failed(self.hook_error(error)))?;
        let request = Request {
            model: settings.model,
            messages: context.to_vec(),
            tools: spec.tools.iter().map(|tool| tool.definition()).collect(),
            max_tokens: spec.max_tokens,
            reasoning: settings.reasoning,
        };
        let request_entry = match self.control.session.as_deref_mut() {
            Some(session) => Some(
                session
                    .record_request(&request, spec.provider.as_ref())
                    .await
                    .map_err(Halt::Unrecorded)?,
            ),
            None => None,
        };
        if self.control.cancel.is_cancelled() {
            return Ok(None);
        }
        self.emit(AgentEvent::RequestStart {
            kind: RequestKind::Task,
            entry_id: request_entry,
            model: request.model.clone(),
            attempt: self.control.attempt_number,
        })
        .await;
        result.model_requests = result.model_requests.saturating_add(1);
        let message = self.stream(request).await;
        result.usage.add(&message.usage);
        Ok(Some(message))
    }

    /// Stream one response, reporting its start and its deltas. The caller records the
    /// response and reports its end.
    async fn stream(&self, request: Request) -> AssistantMessage {
        let spec = &self.config.spec;
        let mut events = spec.provider.stream(request, self.control.cancel.clone());
        let mut started = false;
        while let Some(event) = events.next().await {
            if !started {
                started = true;
                self.emit(AgentEvent::MessageStart { role: "assistant" }).await;
            }
            match event {
                StreamEvent::Start => {}
                StreamEvent::Delta(delta) => self.emit(AgentEvent::MessageUpdate { delta }).await,
                StreamEvent::Done(message) | StreamEvent::Error(message) => return message,
            }
        }
        let stop = if self.control.cancel.is_cancelled() {
            StopReason::Aborted
        } else {
            StopReason::Error
        };
        if !started {
            self.emit(AgentEvent::MessageStart { role: "assistant" }).await;
        }
        AssistantMessage::failure(
            spec.provider.name(),
            &spec.model,
            stop,
            "provider stream ended without a final event",
        )
    }
}
