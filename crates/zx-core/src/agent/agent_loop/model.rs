//! Model request recording and streaming.

use super::{
    AgentEvent, AssistantMessage, Attempt, AttemptResult, Context, Halt, Request, RequestKind, RequestSource,
    StopReason, StreamEvent, StreamExt,
};
use crate::agent::RequestSettings;

impl Attempt<'_> {
    /// Record and send the task request of turn `turn`, then account for its response.
    pub(super) async fn request_model(
        &mut self,
        result: &mut AttemptResult,
        turn: u32,
    ) -> Result<Option<AssistantMessage>, Halt> {
        let config = self.config;
        let messages = self.host.session().messages();
        let configured = RequestSettings {
            model: config.model.clone(),
            reasoning: config.reasoning,
        };
        let settings = config
            .hooks
            .prepare_request(&messages, turn, configured, self.trace.context())
            .await
            .map_err(|error| Halt::Failed(self.report_hook_failure(error)))?;
        let request = Request {
            model: settings.model,
            messages,
            // The active set may have changed since the last request; the audit records
            // what each request declared.
            tools: config
                .tools
                .iter()
                .filter(|tool| config.active_tools.is_active(tool.name()))
                .map(|tool| tool.definition())
                .collect(),
            max_tokens: config.max_tokens,
            reasoning: settings.reasoning,
            cache_key: config.cache_key.clone(),
        };
        let provider = config.provider.as_ref();
        let request_entry = self
            .host
            .session()
            .record_request(
                &request,
                provider.name(),
                provider.request_options(),
                RequestSource::Projection,
            )
            .await
            .context("cannot persist request checkpoint")
            .map_err(Halt::Unrecorded)?;
        if self.trace.cancel().is_cancelled() {
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
        message.usage.price(&config.models, &model);
        result.usage.add(&message.usage);
        Ok(Some(message))
    }

    /// Stream one response, reporting its start and its deltas. The caller records the
    /// response and reports its end.
    async fn stream(&self, request: Request) -> AssistantMessage {
        let config = self.config;
        let mut events = config.provider.stream(request, self.trace.cancel().clone());
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
        let stop = if self.trace.cancel().is_cancelled() {
            StopReason::Aborted
        } else {
            StopReason::Error
        };
        if !started {
            self.emit(AgentEvent::MessageStart { role: "assistant" }).await;
        }
        AssistantMessage::failure(
            config.provider.name(),
            &config.model,
            stop,
            "provider stream ended without a final event",
        )
    }
}
