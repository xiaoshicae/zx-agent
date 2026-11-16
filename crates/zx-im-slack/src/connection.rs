//! The Socket Mode connection: opening it, keepalive pings, and the envelopes that arrive
//! over it, each acknowledged before it is handled.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{Bytes, Message};
use tokio_util::sync::CancellationToken;
use zx_im::{ImError, Inbound, Keepalive, Seen};

use super::SlackChannel;
use super::events::{decisions, message};

pub(crate) type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

impl SlackChannel {
    /// Open the Socket Mode connection at `url`; a failure is reported.
    async fn open_socket(&self, url: &str) -> Option<Socket> {
        let connected = tokio::time::timeout(self.config.request_timeout, tokio_tungstenite::connect_async(url)).await;
        let failure = match connected {
            Ok(Ok((socket, _))) => return Some(socket),
            Ok(Err(error)) => format!("slack: cannot open the Socket Mode connection: {error}"),
            Err(_) => format!(
                "slack: opening the Socket Mode connection took longer than {} s",
                self.config.request_timeout.as_secs()
            ),
        };
        self.report(&ImError::Transport(failure));
        None
    }

    /// Receive over one Socket Mode connection until it ends.
    pub(crate) async fn receive(
        &self,
        url: &str,
        inbound: &mpsc::Sender<Inbound>,
        bot: Option<&str>,
        seen: &mut Seen<(String, String)>,
        cancel: &CancellationToken,
    ) -> Ended {
        let opened = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ended::Cancelled,
            opened = self.open_socket(url) => opened,
        };
        let Some(mut socket) = opened else {
            return Ended::Failed;
        };
        let mut keepalive = Keepalive::new(self.config.ping_interval);
        loop {
            let frame = tokio::select! {
                () = cancel.cancelled() => {
                    // The server is stopping; a failed close changes nothing.
                    socket.close(None).await.ok();
                    return Ended::Cancelled;
                }
                () = tokio::time::sleep_until(keepalive.next_ping()) => {
                    if socket.send(Message::Ping(Bytes::new())).await.is_err() {
                        return Ended::Lost;
                    }
                    keepalive.pinged();
                    continue;
                }
                () = tokio::time::sleep_until(keepalive.deadline()) => return Ended::Silent(keepalive.silence()),
                frame = socket.next() => frame,
            };
            keepalive.alive();
            let text = match frame {
                Some(Ok(Message::Text(text))) => text,
                // tungstenite answers pings itself, and the pongs answer ours; Slack sends no
                // binary frames.
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_))) => continue,
                Some(Ok(Message::Close(_)) | Err(_)) | None => return Ended::Lost,
            };
            let Some(frame) = self.frame_of(&text) else {
                continue;
            };
            if let Err(ended) = acknowledge(&mut socket, &frame).await {
                return ended;
            }
            let events = match envelope(&frame, bot, seen) {
                Envelope::Disconnect => return Ended::Lost,
                Envelope::Events(events) => events,
            };
            for event in events {
                if inbound.send(event).await.is_err() {
                    // The server no longer reads: it is stopping.
                    return Ended::Cancelled;
                }
            }
        }
    }

    /// `text` as a JSON frame; one that is not is reported and dropped.
    fn frame_of(&self, text: &str) -> Option<Value> {
        match serde_json::from_str(text) {
            Ok(frame) => Some(frame),
            Err(error) => {
                self.report(&ImError::Transport(format!(
                    "slack: a Socket Mode frame is not JSON: {error}"
                )));
                None
            }
        }
    }
}

/// Acknowledge the envelope of `frame`, if it has one. Slack redelivers what is not
/// acknowledged in time, so this comes before any handling.
async fn acknowledge(socket: &mut Socket, frame: &Value) -> Result<(), Ended> {
    if let Some(id) = frame["envelope_id"].as_str() {
        let ack = json!({ "envelope_id": id }).to_string();
        socket.send(Message::text(ack)).await.map_err(|_| Ended::Lost)?;
    }
    Ok(())
}

/// The address in the answer of `apps.connections.open`.
pub(crate) fn connection_url(answer: &Value) -> Result<String, ImError> {
    answer["url"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| ImError::Transport("slack apps.connections.open: no url in the answer".into()))
}

pub(crate) enum Ended {
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

/// What `frame` asks for; a redelivered message is left out.
fn envelope(frame: &Value, bot: Option<&str>, seen: &mut Seen<(String, String)>) -> Envelope {
    match frame["type"].as_str() {
        Some("disconnect") => Envelope::Disconnect,
        Some("events_api") => Envelope::Events(
            message(&frame["payload"], bot)
                .filter(|(key, _)| seen.insert(key))
                .map(|(_, event)| event)
                .into_iter()
                .collect(),
        ),
        Some("interactive") => Envelope::Events(decisions(&frame["payload"])),
        _ => Envelope::Events(Vec::new()),
    }
}
