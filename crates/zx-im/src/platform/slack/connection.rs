//! The Socket Mode connection: opening it, keepalive pings, and the envelopes that arrive
//! over it, each acknowledged once its message is kept, before it is handled.

use futures::SinkExt;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::{self, Bytes, Message};
use tokio_util::sync::CancellationToken;

use super::SlackChannel;
use super::events::{Me, decisions, message};
use crate::platform::{Connection, Ended, Keepalive, Next, Socket, keep, next_message, report};
use crate::{ImError, Inbound, Inlet};

impl SlackChannel {
    /// Ask for the Socket Mode connection's address and open it. An error is a failure that
    /// retrying cannot fix.
    pub(super) async fn connect(&self) -> Result<Connection<Socket>, ImError> {
        let answer = match self
            .call("apps.connections.open", &self.config.app_token, &[], None)
            .await
        {
            Ok(answer) => answer,
            Err(error) => match ImError::from(error) {
                error @ ImError::Config(_) => return Err(error),
                ImError::Transport(reason) => return Ok(Connection::Later(reason)),
            },
        };
        let Some(url) = answer["url"].as_str() else {
            return Ok(Connection::Later(
                "slack apps.connections.open: no url in the answer".into(),
            ));
        };
        let connected = tokio::time::timeout(self.config.request_timeout, tokio_tungstenite::connect_async(url)).await;
        Ok(match connected {
            Ok(Ok((socket, _))) => Connection::Open(Box::new(socket)),
            Ok(Err(error)) => Connection::Later(format!("slack: cannot open the Socket Mode connection: {error}")),
            Err(_) => Connection::Later(format!(
                "slack: opening the Socket Mode connection took longer than {:?}",
                self.config.request_timeout
            )),
        })
    }

    /// Receive over one Socket Mode connection until it ends.
    pub(super) async fn receive(
        &self,
        mut socket: Socket,
        inlet: &Inlet,
        me: &Me,
        cancel: &CancellationToken,
    ) -> Ended {
        let mut keepalive = Keepalive::new(self.config.ping_interval);
        // A WebSocket ping, which Slack answers with a pong.
        let ping = Message::Ping(Bytes::new());
        loop {
            let text = match next_message(&mut socket, &mut keepalive, &ping, cancel).await {
                Next::Message(Message::Text(text)) => text,
                // tungstenite answers pings itself, and the pongs answer ours; Slack sends no
                // binary frames.
                Next::Message(Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_)) => continue,
                Next::Message(Message::Close(_)) => return Ended::Closed,
                Next::Lost(why) => return Ended::Lost(why),
                Next::Silent(silence) => return Ended::Silent(silence),
                Next::Cancelled => return Ended::Cancelled,
            };
            let Some(frame) = self.frame_of(&text) else {
                continue;
            };
            let events = match envelope(&frame, me) {
                Envelope::Disconnect => return Ended::Closed,
                Envelope::Events(events) => events,
            };
            let delivery = keep(inlet, events, self.config.on_error.as_ref()).await;
            if let Err(ended) = delivery.finish(inlet, acknowledge(&mut socket, &frame)).await {
                return ended;
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
