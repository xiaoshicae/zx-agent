//! Model request recording and streaming.

use anyhow::Context;
use futures::StreamExt;

use super::{Halt, StepLoop};
use crate::ai::{AssistantMessage, Request, StopReason, StreamEvent};
use crate::events::{EventKind, RequestKind};
use crate::hooks::RequestSettings;
use crate::transcript::RequestSource;

impl StepLoop<'_> {
    /// Record and send the task request of step `step`, then account for its response.
    pub(super) async fn request_model(&mut self, step: u32) -> Result<AssistantMessage, Halt> {
        let config = self.run.config;
        let messages = self.run.transcript.messages();
        let configured = RequestSettings {
            model: config.model.clone(),
            reasoning: config.reasoning,
        };
        let prepared = config
            .hooks
            .prepare_request(&messages, step, configured, &self.run.target.trace.context())
            .await;
        let settings = match prepared {
            Ok(settings) => settings,
            Err(error) => return Err(self.hook_failed(error).await),
        };
        self.accounting
            .check_model(&config.models, &settings.model)
            .map_err(Halt::Failed)?;
        let request = Request {
            model: settings.model,
            messages,
            // The active set may have changed since the last request; the audit records
            // what each request declared.
            tools: config.tools.definitions(&config.active_tools),
            max_tokens: config.max_tokens,
            reasoning: settings.reasoning,
            cache_key: config.cache_key.clone(),
        };
        let provider = config.provider.as_ref();
        let request_entry = self
            .run
            .transcript
            .record_request(
                &request,
                provider.name(),
                provider.request_options(),
                RequestSource::Task,
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
        let config = self.run.config;
        let mut events = config.provider.stream(request, self.run.target.trace.cancel().clone());
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
        let stop = if self.run.target.trace.cancel().is_cancelled() {
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
