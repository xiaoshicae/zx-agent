//! Model request recording and streaming.

use super::{
    AgentEvent, AssistantMessage, Engine, Halt, LoopResult, Message, Request, RequestKind, StopReason, StreamEvent,
    StreamExt,
};
use crate::agent::RequestSettings;

impl Engine<'_, '_> {
    /// Record and send the task request of turn `turn`, then account for its response.
    pub(super) async fn request_model(
        &mut self,
        context: &[Message],
        result: &mut LoopResult,
        turn: u32,
    ) -> Result<Option<AssistantMessage>, Halt> {
        let spec = &self.config.spec;
        let configured = RequestSettings {
            model: spec.model.clone(),
            reasoning: spec.reasoning,
        };
        let settings = self
            .config
            .hooks
            .prepare_request(context, turn, configured, self.trace.context())
            .await
            .map_err(|error| Halt::Failed(self.report_hook_failure(error)))?;
        let request = Request {
            model: settings.model,
            messages: context.to_vec(),
            // The active set may have changed since the last request; the audit records
            // what each request declared.
            tools: spec
                .tools
                .iter()
                .filter(|tool| self.config.active_tools.is_active(tool.name()))
                .map(|tool| tool.definition())
                .collect(),
            max_tokens: spec.max_tokens,
            reasoning: settings.reasoning,
            cache_key: self.config.cache_key.clone(),
        };
        let request_entry = self
            .session
            .record_request(&request, spec.provider.as_ref())
            .await
            .map_err(Halt::Unrecorded)?;
        if self.cancel.is_cancelled() {
            return Ok(None);
        }
        self.emit(AgentEvent::RequestStart {
            kind: RequestKind::Task,
            entry_id: request_entry,
            model: request.model.clone(),
            attempt: self.attempt_number,
        })
        .await;
        result.model_requests = result.model_requests.saturating_add(1);
        let model = request.model.clone();
        let mut message = self.stream(request).await;
        message.usage.price(&self.config.models, &model);
        result.usage.add(&message.usage);
        Ok(Some(message))
    }

    /// Stream one response, reporting its start and its deltas. The caller records the
    /// response and reports its end.
    async fn stream(&self, request: Request) -> AssistantMessage {
        let spec = &self.config.spec;
        let mut events = spec.provider.stream(request, self.cancel.clone());
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
        let stop = if self.cancel.is_cancelled() {
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
