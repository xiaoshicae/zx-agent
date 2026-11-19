//! The Socket Mode connection: opening it, keepalive pings, and the envelopes that arrive
//! over it, each acknowledged once its message is kept, before it is handled.

use std::time::Duration;

use futures::SinkExt;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::{self, Bytes, Message};
use tokio_util::sync::CancellationToken;

use crate::platform::{Halted, Keepalive, Next, Socket, keep, next_message, report};
use crate::{ImError, Inbound, Inlet};

use super::SlackChannel;
use super::events::{Me, decisions, message};

impl SlackChannel {
    /// Open the Socket Mode connection at `url`; a failure is reported.
    async fn open_socket(&self, url: &str) -> Option<Socket> {
        let connected = tokio::time::timeout(self.config.request_timeout, tokio_tungstenite::connect_async(url)).await;
        let failure = match connected {
            Ok(Ok((socket, _))) => return Some(socket),
            Ok(Err(error)) => format!("slack: cannot open the Socket Mode connection: {error}"),
            Err(_) => format!(
                "slack: opening the Socket Mode connection took longer than {:?}",
                self.config.request_timeout
            ),
        };
        report(self.config.on_error.as_ref(), &ImError::Transport(failure));
        None
    }

    /// Receive over one Socket Mode connection until it ends.
    pub(super) async fn receive(&self, url: &str, inlet: &Inlet, me: &Me, cancel: &CancellationToken) -> Ended {
        let opened = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ended::Cancelled,
            opened = self.open_socket(url) => opened,
        };
        let Some(mut socket) = opened else {
            return Ended::Failed;
        };
        let mut keepalive = Keepalive::new(self.config.ping_interval);
        // A WebSocket ping, which Slack answers with a pong.
        let ping = Message::Ping(Bytes::new());
        loop {
            let text = match next_message(&mut socket, &mut keepalive, &ping, cancel).await {
                Next::Message(Message::Text(text)) => text,
                // tungstenite answers pings itself, and the pongs answer ours; Slack sends no
                // binary frames.
                Next::Message(Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_)) => continue,
                Next::Message(Message::Close(_)) | Next::Lost => return Ended::Lost,
                Next::Silent(silence) => return Ended::Silent(silence),
                Next::Cancelled => return Ended::Cancelled,
            };
            let Some(frame) = self.frame_of(&text) else {
                continue;
            };
            let events = match envelope(&frame, me) {
                Envelope::Disconnect => return Ended::Lost,
                Envelope::Events(events) => events,
            };
            let delivery = keep(inlet, events, self.config.on_error.as_ref()).await;
            match delivery.finish(inlet, acknowledge(&mut socket, &frame)).await {
                Ok(()) => {}
                Err(Halted::ServerStopped) => return Ended::Cancelled,
                Err(Halted::Unacknowledged) => return Ended::Lost,
            }
        }
    }

    /// `text` as a JSON frame; one that is not is reported and dropped.
    fn frame_of(&self, text: &str) -> Option<Value> {
        match serde_json::from_str(text) {
            Ok(frame) => Some(frame),
            Err(error) => {
                report(
                    self.config.on_error.as_ref(),
                    &ImError::Transport(format!("slack: a Socket Mode frame is not JSON: {error}")),
                );
                None
            }
        }
    }
}

/// Acknowledge the envelope of `frame`, if it has one. Slack redelivers what is not
/// acknowledged in time, so this comes as soon as its message is kept, before any handling.
async fn acknowledge(socket: &mut Socket, frame: &Value) -> Result<(), tungstenite::Error> {
    if let Some(id) = frame["envelope_id"].as_str() {
        let ack = json!({ "envelope_id": id }).to_string();
        socket.send(Message::text(ack)).await?;
    }
    Ok(())
}

/// The address in the answer of `apps.connections.open`.
pub(super) fn connection_url(answer: &Value) -> Result<String, ImError> {
    answer["url"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| ImError::Transport("slack apps.connections.open: no url in the answer".into()))
}

pub(super) enum Ended {
    Cancelled,
    /// Slack asked to reconnect, or the connection dropped.
    Lost,
    /// Nothing came over the connection for this long, not even answers to the pings.
    Silent(Duration),
    /// The WebSocket could not be opened.
    Failed,
}

/// What a Socket Mode envelope asks for.
enum Envelope {
    /// Slack closes the connection soon; another is to be opened.
    Disconnect,
    /// The messages and clicks to pass on: none for `hello`, and for envelope types this bot
    /// does not subscribe to.
    Events(Vec<Inbound>),
}

/// What `frame` asks for.
fn envelope(frame: &Value, me: &Me) -> Envelope {
    match frame["type"].as_str() {
        Some("disconnect") => Envelope::Disconnect,
        Some("events_api") => Envelope::Events(message(&frame["payload"], me).into_iter().collect()),
        Some("interactive") => Envelope::Events(decisions(&frame["payload"])),
        _ => Envelope::Events(Vec::new()),
    }
}
