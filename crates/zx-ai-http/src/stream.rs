//! The stream of one request: the data of each Server-Sent Event of the response, read within
//! the provider's limits, and the end of the provider's own stream with the message made of
//! the answer, whatever happened.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use zx_ai::{
    ABORTED_MESSAGE, AssistantMessage, EventSender, ProviderError, ProviderErrorKind, StopReason, StreamEvent,
    duration_ms, send_event,
};

use crate::failure::{Failure, transport_failure};
use crate::sse::SseParser;

/// The data of each event of a streaming response, in order, read as the caller asks for it.
pub struct Events {
    response: reqwest::Response,
    parser: SseParser,
    idle_timeout: Duration,
    max_response_bytes: usize,
    received_bytes: usize,
    /// Events read and not yet asked for.
    ready: VecDeque<String>,
    /// Whether the body has ended: what is left is in `ready`.
    ended: bool,
}

impl Events {
    /// The events of `response`, whose body may take `max_response_bytes` in all and each of
    /// its events `max_sse_event_bytes`, with no more than `idle_timeout` without data.
    pub fn new(
        response: reqwest::Response,
        idle_timeout: Duration,
        max_response_bytes: usize,
        max_sse_event_bytes: usize,
    ) -> Self {
        Self {
            response,
            parser: SseParser::new(max_sse_event_bytes),
            idle_timeout,
            max_response_bytes,
            received_bytes: 0,
            ready: VecDeque::new(),
            ended: false,
        }
    }

    /// The data of the next event, or `None` at the end of the body. Fails when no data comes
    /// within the idle timeout, when the body or one of its events is over its byte limit, and
    /// when the read fails.
    pub async fn next(&mut self) -> Result<Option<String>, Failure> {
        loop {
            if let Some(data) = self.ready.pop_front() {
                return Ok(Some(data));
            }
            if self.ended {
                return Ok(None);
            }
            let idle = self.idle_timeout;
            let chunk = tokio::time::timeout(idle, self.response.chunk())
                .await
                .map_err(|_| {
                    Failure::error(
                        ProviderErrorKind::Timeout,
                        format!("stream stalled: no data for {idle:?}"),
                    )
                })?
                .map_err(|error| transport_failure("stream read failed", &error))?;
            let Some(chunk) = chunk else {
                self.ended = true;
                self.ready.extend(self.parser.finish());
                continue;
            };
            if chunk.len() > self.max_response_bytes.saturating_sub(self.received_bytes) {
                return Err(Failure::error(
                    ProviderErrorKind::ResourceLimit,
                    format!("stream response exceeds the {} byte limit", self.max_response_bytes),
                ));
            }
            self.received_bytes += chunk.len();
            let events = self
                .parser
                .push(&chunk)
                .map_err(|error| Failure::error(ProviderErrorKind::ResourceLimit, error))?;
            self.ready.extend(events);
        }
    }
}

/// What a provider makes of the answer streamed so far.
pub trait Answer {
    /// The complete message. Fails on a stream that ended without saying how the answer
    /// ended, which [`end`] reports as broken off.
    fn finish(&self) -> Result<AssistantMessage, String>;

    /// The message so far, ended by `stop_reason` with `error` and its `provider_error`.
    fn failure(
        &self,
        stop_reason: StopReason,
        error: impl Into<String>,
        provider_error: Option<ProviderError>,
    ) -> AssistantMessage;
}

/// End the provider's stream with the message that `outcome` makes of `answer`, timed from
/// `started`: the complete message, or the answer so far with what ended it, which keeps the
/// content streamed before the failure. Nothing is sent to a consumer that went away.
pub async fn end(outcome: Result<(), Failure>, answer: &impl Answer, started: Instant, tx: &mut EventSender) {
    let mut message = match outcome {
        // A stream that ends before saying how the answer ended broke off, as when a gateway
        // or proxy cuts it: asking again may get the whole answer (pi: `connection.?closed`).
        Ok(()) => answer.finish().unwrap_or_else(|error| {
            answer.failure(StopReason::Error, error, Some(ProviderErrorKind::Transport.into()))
        }),
        Err(Failure::Aborted) => answer.failure(StopReason::Aborted, ABORTED_MESSAGE, None),
        Err(Failure::Closed) => return,
        Err(Failure::Error {
            provider_error,
            message,
        }) => answer.failure(StopReason::Error, message, Some(provider_error)),
    };
    message.duration_ms = Some(duration_ms(started.elapsed()));
    // Once the consumer closes, a terminal event has nobody left to report to.
    send_event(tx, StreamEvent::ending(message)).await;
}
