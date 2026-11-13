//! Model request recording and streaming.

use anyhow::Context;
use futures::StreamExt;

use super::{Halt, StepLoop};
use crate::ai::{AssistantMessage, Request, StopReason, StreamEvent};
use crate::transcript::RequestSource;
use crate::{EventKind, RequestKind, RequestSettings};

impl StepLoop<'_> {
    /// Record and send the task request of step `step`, then account for its response.
    pub(super) async fn request_model(&mut self, step: u32) -> Result<AssistantMessage, Halt> {
        let config = self.config;
        let messages = self.host.session().messages();
        let configured = RequestSettings {
            model: config.model.clone(),
            reasoning: config.reasoning,
        };
        let settings = config
            .hooks
            .prepare_request(&messages, step, configured, self.trace.context())
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
        self.ensure_not_cancelled()?;
        self.emit(EventKind::RequestStart {
            kind: RequestKind::Task,
            entry_id: request_entry,
            model: request.model.clone(),
            attempt: self.accounting.attempts,
        })
        .await;
        self.accounting.model_requests = self.accounting.model_requests.saturating_add(1);
        let model = request.model.clone();
        let mut message = self.stream(request).await;
        message.usage.price(&config.models, &model);
        self.accounting.usage.add(&message.usage);
        Ok(message)
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
                self.emit(EventKind::MessageStart { role: "assistant" }).await;
            }
            match event {
                StreamEvent::Start => {}
                StreamEvent::Delta(delta) => self.emit(EventKind::MessageUpdate { delta }).await,
                StreamEvent::Done(message) | StreamEvent::Error(message) => return message,
            }
        }
        let stop = if self.trace.cancel().is_cancelled() {
            StopReason::Aborted
        } else {
            StopReason::Error
        };
        if !started {
            self.emit(EventKind::MessageStart { role: "assistant" }).await;
        }
        AssistantMessage::failure(
            config.provider.name(),
            &config.model,
            stop,
            "provider stream ended without a final event",
        )
    }
}
