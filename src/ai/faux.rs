//! Scripted provider for tests (pi: packages/ai/src/providers/faux.ts). No network, no keys.

use std::collections::VecDeque;
use std::sync::Mutex;

use futures::channel::mpsc::unbounded;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::provider::{EventReceiver, Provider, send_event};
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
        lock(&self.requests).push(request);
        let step = lock(&self.steps).pop_front();
        let (tx, rx) = unbounded();
        tokio::spawn(async move {
            let failure = |stop, error: &str| AssistantMessage::failure("faux", &model, stop, error);
            let event = match step {
                None => StreamEvent::Error(failure(StopReason::Error, "faux provider: no scripted response left")),
                Some(FauxStep::Fail(error)) => StreamEvent::Error(failure(StopReason::Error, &error)),
                Some(FauxStep::Hang) => {
                    cancel.cancelled().await;
                    StreamEvent::Error(failure(StopReason::Aborted, "Request was aborted"))
                }
                Some(FauxStep::Respond { blocks, stop }) => {
                    let timestamp = now_ms();
                    send_event(&tx, StreamEvent::Start);
                    for block in &blocks {
                        for delta in deltas_for(block) {
                            send_event(&tx, StreamEvent::Delta(delta));
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
                        timestamp,
                        duration_ms: Some(0),
                    };
                    match message.stop_reason {
                        StopReason::Error | StopReason::Aborted => StreamEvent::Error(message),
                        StopReason::Stop | StopReason::Length | StopReason::ToolUse => StreamEvent::Done(message),
                    }
                }
            };
            send_event(&tx, event);
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
    async fn requests_beyond_the_script_fail() {
        let provider = FauxProvider::new(Vec::<FauxStep>::new());
        let events = events(&provider).await;
        assert!(matches!(
            events.as_slice(),
            [StreamEvent::Error(message)] if message.stop_reason == StopReason::Error
        ));
    }
}
