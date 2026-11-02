//! Scripted provider for tests (pi: packages/ai/src/providers/faux.ts). No network, no keys.

use std::collections::VecDeque;
use std::sync::Mutex;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::provider::{EventReceiver, Provider, event_channel, send_event};
use super::types::{AssistantMessage, ContentBlock, Delta, Request, StopReason, StreamEvent, Usage, now_ms};
use crate::sync::lock;

/// One scripted response.
pub enum FauxStep {
    /// Stream these blocks, then finish with `stop`.
    Respond {
        blocks: Vec<ContentBlock>,
        stop: StopReason,
    },
    /// Fail before streaming, like an HTTP error.
    Fail(String),
    /// Wait until cancelled, then end with `Aborted`.
    Hang,
    /// Decide the step from the request, for answers that must refer to what the request
    /// holds, such as the entry IDs a summary request offers. It may not compute another.
    Compute(Box<dyn FnOnce(&Request) -> FauxStep + Send>),
}

impl FauxStep {
    /// Answer with text and stop.
    pub fn text(text: impl Into<String>) -> Self {
        FauxStep::Respond {
            blocks: vec![ContentBlock::Text { text: text.into() }],
            stop: StopReason::Stop,
        }
    }

    /// Answer with text cut off at the output token limit.
    pub fn truncated(text: impl Into<String>) -> Self {
        FauxStep::Respond {
            blocks: vec![ContentBlock::Text { text: text.into() }],
            stop: StopReason::Length,
        }
    }

    /// Request one tool call.
    pub fn tool_call(id: &str, name: &str, arguments: Value) -> Self {
        Self::tool_calls(vec![(id, name, arguments)])
    }

    /// Decide the step when the request arrives, from the request.
    pub fn compute(answer: impl FnOnce(&Request) -> FauxStep + Send + 'static) -> Self {
        FauxStep::Compute(Box::new(answer))
    }

    /// Request several tool calls, given as `(id, name, arguments)`.
    pub fn tool_calls(calls: Vec<(&str, &str, Value)>) -> Self {
        FauxStep::Respond {
            blocks: calls
                .into_iter()
                .map(|(id, name, arguments)| ContentBlock::ToolCall {
                    id: id.to_string(),
                    name: name.to_string(),
                    arguments,
                    arguments_error: None,
                })
                .collect(),
            stop: StopReason::ToolUse,
        }
    }
}

/// Provider that replays scripted steps, one per request, and records every request.
pub struct FauxProvider {
    steps: Mutex<VecDeque<FauxStep>>,
    requests: Mutex<Vec<Request>>,
}

impl FauxProvider {
    /// Create a provider that answers requests with `steps` in order. Requests beyond the
    /// script fail with an error message.
    pub fn new(steps: impl IntoIterator<Item = FauxStep>) -> Self {
        Self {
            steps: Mutex::new(steps.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// Requests received so far, in order.
    pub fn requests(&self) -> Vec<Request> {
        lock(&self.requests).clone()
    }

    /// Scripted steps not used yet.
    pub fn remaining(&self) -> usize {
        lock(&self.steps).len()
    }
}

impl Provider for FauxProvider {
    fn name(&self) -> &str {
        "faux"
    }

    fn stream(&self, request: Request, cancel: CancellationToken) -> EventReceiver {
        let model = request.model.clone();
        // Taken first, so that the lock is released before a computed step runs.
        let next = lock(&self.steps).pop_front();
        let step = match next {
            Some(FauxStep::Compute(answer)) => Some(answer(&request)),
            step => step,
        };
        lock(&self.requests).push(request);
        let (mut tx, rx) = event_channel();
        tokio::spawn(async move {
            let failure = |stop, error: &str| AssistantMessage::failure("faux", &model, stop, error);
            let event = match step {
                None => StreamEvent::Error(failure(StopReason::Error, "faux provider: no scripted response left")),
                Some(FauxStep::Fail(error)) => StreamEvent::Error(failure(StopReason::Error, &error)),
                Some(FauxStep::Compute(_)) => StreamEvent::Error(failure(
                    StopReason::Error,
                    "faux provider: a computed step computed another",
                )),
                Some(FauxStep::Hang) => {
                    cancel.cancelled().await;
                    StreamEvent::Error(failure(StopReason::Aborted, "Request was aborted"))
                }
                Some(FauxStep::Respond { blocks, stop }) => {
                    let timestamp = now_ms();
                    if !send_event(&mut tx, StreamEvent::Start).await {
                        return;
                    }
                    for block in &blocks {
                        for delta in deltas_for(block) {
                            if !send_event(&mut tx, StreamEvent::Delta(delta)).await {
                                return;
                            }
                        }
                    }
                    let aborted = cancel.is_cancelled();
                    let message = AssistantMessage {
                        content: blocks,
                        provider: "faux".to_string(),
                        model: model.clone(),
                        usage: Usage {
                            input: 10,
                            output: 5,
                            cache_read: 0,
                            total_tokens: 15,
                        },
                        stop_reason: if aborted { StopReason::Aborted } else { stop },
                        error_message: aborted.then(|| "Request was aborted".to_string()),
                        provider_error: None,
                        timestamp,
                        duration_ms: Some(0),
                    };
                    match message.stop_reason {
                        StopReason::Error | StopReason::Aborted => StreamEvent::Error(message),
                        StopReason::Stop | StopReason::Length | StopReason::ToolUse => StreamEvent::Done(message),
                    }
                }
            };
            // A failed terminal send only means its consumer has gone away.
            send_event(&mut tx, event).await;
        });
        rx
    }
}

fn deltas_for(block: &ContentBlock) -> Vec<Delta> {
    match block {
        ContentBlock::Text { text } => vec![Delta::Text { text: text.clone() }],
        ContentBlock::Thinking { thinking, .. } => vec![Delta::Thinking { text: thinking.clone() }],
        ContentBlock::ToolCall {
            id, name, arguments, ..
        } => vec![
            Delta::ToolCallStart {
                id: id.clone(),
                name: name.clone(),
            },
            Delta::ToolCallArgs {
                id: id.clone(),
                text: arguments.to_string(),
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;

    use super::*;

    async fn events(provider: &FauxProvider) -> Vec<StreamEvent> {
        let request = Request {
            model: "m".into(),
            messages: vec![],
            tools: vec![],
            max_tokens: None,
        };
        provider.stream(request, CancellationToken::new()).collect().await
    }

    #[tokio::test]
    async fn a_scripted_answer_streams_start_deltas_and_done() {
        let provider = FauxProvider::new([FauxStep::text("hi")]);
        let events = events(&provider).await;
        assert!(matches!(
            events.as_slice(),
            [StreamEvent::Start, StreamEvent::Delta(Delta::Text { .. }), StreamEvent::Done(message)]
                if message.text() == "hi"
        ));
        assert_eq!(provider.requests().len(), 1);
    }

    #[tokio::test]
    async fn a_computed_step_answers_from_the_request() {
        let provider = FauxProvider::new([FauxStep::compute(|request| {
            FauxStep::text(format!("answer for {}", request.model))
        })]);
        let events = events(&provider).await;
        assert!(matches!(
            events.last(),
            Some(StreamEvent::Done(message)) if message.text() == "answer for m"
        ));
        assert_eq!(provider.requests().len(), 1);
    }

    #[tokio::test]
    async fn a_computed_step_cannot_compute_another() {
        let provider = FauxProvider::new([FauxStep::compute(|_| FauxStep::compute(|_| FauxStep::text("never")))]);
        let events = events(&provider).await;
        assert!(matches!(
            events.as_slice(),
            [StreamEvent::Error(message)] if message.stop_reason == StopReason::Error
        ));
    }

    #[tokio::test]
    async fn requests_beyond_the_script_fail() {
        let provider = FauxProvider::new(Vec::<FauxStep>::new());
        let events = events(&provider).await;
        assert!(matches!(
            events.as_slice(),
            [StreamEvent::Error(message)] if message.stop_reason == StopReason::Error
        ));
    }

    #[tokio::test]
    async fn a_burst_larger_than_the_buffer_keeps_every_delta_and_the_final_message() {
        let count = crate::ai::provider::STREAM_EVENT_BUFFER * 3;
        let blocks = (0..count).map(|_| ContentBlock::Text { text: "x".into() }).collect();
        let provider = FauxProvider::new([FauxStep::Respond {
            blocks,
            stop: StopReason::Stop,
        }]);
        let request = Request {
            model: "m".into(),
            messages: vec![],
            tools: vec![],
            max_tokens: None,
        };
        let mut receiver = provider.stream(request, CancellationToken::new());
        let mut deltas = 0;
        let mut final_text = None;
        while let Some(event) = receiver.next().await {
            match event {
                StreamEvent::Delta(_) => deltas += 1,
                StreamEvent::Done(message) => final_text = Some(message.text()),
                StreamEvent::Error(message) => panic!("unexpected provider error: {message:?}"),
                StreamEvent::Start => {}
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(deltas, count);
        assert_eq!(final_text, Some("x".repeat(count)));
    }
}
