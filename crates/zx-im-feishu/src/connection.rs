//! The long connection: its address and settings, opening it, pings and the silence that
//! means it died, acknowledging events, and joining the fragments of one message. The
//! frames themselves are in `frame.rs`.

use std::collections::HashMap;
use std::fmt::Display;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_util::sync::CancellationToken;
use zx_im::{ImError, Inbound, Keepalive, MessageId, Seen};

use super::frame::Frame;
use super::{FeishuChannel, MAX_INTERVAL};

/// The prefix of every error of the long connection.
pub(crate) const PREFIX: &str = "feishu long connection";
/// Where the long connection's address is requested (Go SDK `ws/const.go`).
const ENDPOINT_PATH: &str = "/callback/ws/endpoint";
/// Frame methods (Go SDK `ws/const.go`): control frames carry pings and pongs.
const CONTROL: i32 = 0;
const DATA: i32 = 1;
/// Endpoint codes the Go SDK retries: 1 (system busy) and 1000040343. Any other non-zero
/// code ends the client (`ws/client_transport.go`).
const RETRY_CODES: [i64; 2] = [1, 1_000_040_343];
/// Handshake refusal for too many connections, with status 514 (`ws/const.go`).
const EXCEED_CONN_LIMIT: &str = "1000040350";
/// Ping interval until the server sends its own (`ws/client.go`).
const DEFAULT_PING_INTERVAL: Duration = Duration::from_secs(120);
/// Fragments of one message wait this long for the rest; the Go SDK keeps them for five
/// seconds after each fragment (`ws/client_message.go`).
const FRAGMENT_TTL: Duration = Duration::from_secs(5);
/// Most messages with fragments waiting at once, and most fragments of one message.
const MAX_PARTIAL: usize = 64;
const MAX_FRAGMENTS: usize = 64;
/// What the acknowledgement of an event carries: the Go SDK's answer for a handled event.
const ACK_PAYLOAD: &[u8] = br#"{"code":200,"headers":null,"data":null}"#;

/// Settings the server sends with the address and in pongs.
#[derive(Clone, Copy)]
pub(crate) struct ClientConfig {
    pub(crate) ping: Duration,
    /// Wait between reconnection attempts; until the server sets it, a backoff is used.
    pub(crate) reconnect: Option<Duration>,
    /// Random delay before the first reconnection, at most this long.
    pub(crate) nonce: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            ping: DEFAULT_PING_INTERVAL,
            reconnect: None,
            nonce: Duration::ZERO,
        }
    }
}

impl ClientConfig {
    /// Apply what `value` sets. A missing or zero interval keeps the current one: the Go
    /// SDK would set it to zero.
    fn update(&mut self, value: &Value) {
        let seconds = |key: &str| {
            value[key]
                .as_u64()
                .filter(|secs| *secs > 0)
                .map(|secs| Duration::from_secs(secs).min(MAX_INTERVAL))
        };
        if let Some(ping) = seconds("PingInterval") {
            self.ping = ping;
        }
        if let Some(reconnect) = seconds("ReconnectInterval") {
            self.reconnect = Some(reconnect);
        }
        if let Some(nonce) = value["ReconnectNonce"].as_u64() {
            self.nonce = Duration::from_secs(nonce).min(MAX_INTERVAL);
        }
    }
}

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub(crate) enum Ended {
    Cancelled,
    /// The connection dropped or went silent.
    Lost,
}

/// How an attempt to open the long connection went.
pub(crate) enum Connection {
    /// The link is boxed: the other variant is a short reason.
    Open(Box<Link>),
    /// A failure worth retrying, with the reason; the address, which carries a ticket, is
    /// never in it.
    Later(String),
}

/// A failure worth retrying: `reason` after [`PREFIX`].
fn later(reason: impl Display) -> Connection {
    Connection::Later(format!("{PREFIX}: {reason}"))
}

/// One open long connection, and what it keeps while frames come in.
pub(crate) struct Link {
    socket: Socket,
    /// The `service_id` of the address, which pings carry.
    service: i32,
    fragments: Fragments,
    /// The first ping goes out at once, and twice the ping interval plus a margin of
    /// silence counts as a dead connection (`ws/client_session.go`).
    keepalive: Keepalive,
}

impl Link {
    fn new(socket: Socket, service: i32, ping: Duration) -> Self {
        Self {
            socket,
            service,
            fragments: Fragments::default(),
            keepalive: Keepalive::new(ping),
        }
    }
}

/// What a frame from the server carries.
enum Received {
    /// The settings in a pong.
    Settings(Value),
    /// An event, or a fragment of one.
    Event(Frame),
    /// Pings from the server, empty pongs, and `card` frames, which the Go SDK drops: card
    /// clicks arrive as events.
    Other,
}

impl FeishuChannel {
    /// Ask for the long connection's address and open it. An error is a failure that
    /// retrying cannot fix.
    pub(crate) async fn connect(&self, config: &mut ClientConfig) -> Result<Connection, ImError> {
        let url = format!("{}{ENDPOINT_PATH}", self.config.api_base.trim_end_matches('/'));
        let body = json!({
            "AppID": self.config.app_id,
            "AppSecret": self.config.app_secret,
            "ClientAssertion": "",
        });
        // `locale` as the Go SDK's `fetchEndpoint` sends it (`ws/client_transport.go`); it
        // decides the language of `msg` in the answer, which ends up in the error.
        let response = match self.http.post(&url).header("locale", "zh").json(&body).send().await {
            Ok(response) if response.status() == StatusCode::OK => response,
            Ok(response) => return Ok(later(format!("HTTP {}", response.status()))),
            Err(error) => return Ok(later(error)),
        };
        let answer = match response.json::<Value>().await {
            Ok(answer) => answer,
            Err(error) => return Ok(later(format!("the answer is not JSON: {error}"))),
        };
        match answer["code"].as_i64() {
            Some(0) => {}
            Some(code) if RETRY_CODES.contains(&code) => return Ok(later(format!("code {code}, retrying"))),
            code => {
                let detail = answer["msg"].as_str().unwrap_or("no message");
                return Err(ImError::Config(format!("{PREFIX}: code {code:?}: {detail}")));
            }
        }
        // The address carries a ticket: it is used, never shown.
        let Some(address) = answer["data"]["URL"].as_str() else {
            return Ok(later("no address in the answer"));
        };
        config.update(&answer["data"]["ClientConfig"]);
        self.open_socket(address, config.ping).await
    }

    /// Open the WebSocket at `address`, to be pinged every `ping`. A refused handshake is a
    /// configuration error when the platform says the app may not connect, or has too many
    /// connections.
    async fn open_socket(&self, address: &str, ping: Duration) -> Result<Connection, ImError> {
        let service = service_id(address);
        match tokio::time::timeout(self.config.request_timeout, tokio_tungstenite::connect_async(address)).await {
            Ok(Ok((socket, _))) => Ok(Connection::Open(Box::new(Link::new(socket, service, ping)))),
            Ok(Err(tungstenite::Error::Http(response))) => {
                let status = response.status();
                // A code that is not text is as good as none.
                let auth = response
                    .headers()
                    .get("Handshake-Autherrcode")
                    .and_then(|value| value.to_str().ok());
                if status == StatusCode::FORBIDDEN || (status.as_u16() == 514 && auth == Some(EXCEED_CONN_LIMIT)) {
                    Err(ImError::Config(format!(
                        "{PREFIX} refused: HTTP {status}, auth code {}",
                        auth.unwrap_or("none")
                    )))
                } else {
                    Ok(later(format!("handshake refused, HTTP {status}")))
                }
            }
            Ok(Err(error)) => Ok(later(format!("cannot connect: {error}"))),
            Err(_) => Ok(later(format!(
                "connecting took longer than {} s",
                self.config.request_timeout.as_secs()
            ))),
        }
    }

    /// Receive over one connection until it ends.
    pub(crate) async fn receive(
        &self,
        mut link: Link,
        config: &mut ClientConfig,
        inbound: &mpsc::Sender<Inbound>,
        seen: &mut Seen<MessageId>,
        cancel: &CancellationToken,
    ) -> Ended {
        loop {
            let message = tokio::select! {
                () = cancel.cancelled() => {
                    // The server is stopping; a failed close changes nothing.
                    link.socket.close(None).await.ok();
                    return Ended::Cancelled;
                }
                () = tokio::time::sleep_until(link.keepalive.next_ping()) => {
                    if link.socket.send(Message::binary(ping(link.service).encode())).await.is_err() {
                        return Ended::Lost;
                    }
                    link.keepalive.pinged();
                    continue;
                }
                () = tokio::time::sleep_until(link.keepalive.deadline()) => return Ended::Lost,
                message = link.socket.next() => message,
            };
            link.keepalive.alive();
            let bytes = match message {
                Some(Ok(Message::Binary(bytes))) => bytes,
                Some(Ok(Message::Close(_)) | Err(_)) | None => return Ended::Lost,
                // The Go SDK reads only binary messages; anything else still shows life.
                Some(Ok(Message::Text(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => continue,
            };
            let frame = match Frame::decode(&bytes) {
                Ok(frame) => frame,
                Err(error) => {
                    self.report(&ImError::Transport(format!("{PREFIX}: {error}")));
                    continue;
                }
            };
            match received(frame) {
                Received::Settings(settings) => {
                    config.update(&settings);
                    // A new interval counts from the last ping, not after the old one.
                    link.keepalive.set_interval(config.ping);
                }
                Received::Event(frame) => {
                    if let Err(ended) = self.deliver(&mut link, frame, inbound, seen).await {
                        return ended;
                    }
                }
                Received::Other => {}
            }
        }
    }

    /// Acknowledge an event frame and pass its events on, once its message is complete.
    /// `Err` says how the connection ended.
    async fn deliver(
        &self,
        link: &mut Link,
        mut frame: Frame,
        inbound: &mpsc::Sender<Inbound>,
        seen: &mut Seen<MessageId>,
    ) -> Result<(), Ended> {
        // Taken out, so that a fragment is kept, and a whole message delivered, without a copy.
        let payload = std::mem::take(&mut frame.payload);
        let Some(payload) = link.fragments.add(&frame, payload, Instant::now()) else {
            return Ok(());
        };
        // Acknowledge first: Feishu redelivers what is not acknowledged in time.
        link.socket
            .send(Message::binary(acknowledge(&frame).encode()))
            .await
            .map_err(|_| Ended::Lost)?;
        for event in self.events(&payload, seen) {
            // The server no longer reads: it is stopping.
            inbound.send(event).await.map_err(|_| Ended::Cancelled)?;
        }
        Ok(())
    }
}

/// What `frame` carries.
fn received(frame: Frame) -> Received {
    match (frame.method, frame.header("type")) {
        (CONTROL, Some("pong")) if !frame.payload.is_empty() => match serde_json::from_slice(&frame.payload) {
            Ok(settings) => Received::Settings(settings),
            // Settings that cannot be read leave the current ones, as an empty pong does.
            Err(_) => Received::Other,
        },
        (DATA, Some("event")) => Received::Event(frame),
        _ => Received::Other,
    }
}

/// The ping that keeps the long connection alive.
fn ping(service: i32) -> Frame {
    Frame {
        service,
        method: CONTROL,
        headers: vec![("type".into(), "ping".into())],
        ..Frame::default()
    }
}

/// The acknowledgement of a data frame: the frame itself, with the time spent appended as
/// `biz_rt` and a success code as payload (`ws/client_message.go`).
fn acknowledge(frame: &Frame) -> Frame {
    let mut headers = frame.headers.clone();
    // Acknowledged on arrival, before any handling.
    headers.push(("biz_rt".into(), "0".into()));
    Frame {
        seq_id: frame.seq_id,
        log_id: frame.log_id,
        service: frame.service,
        method: frame.method,
        headers,
        payload_encoding: frame.payload_encoding.clone(),
        payload_type: frame.payload_type.clone(),
        payload: ACK_PAYLOAD.to_vec(),
        log_id_new: frame.log_id_new.clone(),
    }
}

/// The `service_id` query parameter of the connection address, which pings carry; 0 when
/// absent or not a number.
fn service_id(address: &str) -> i32 {
    address
        .split_once('?')
        .into_iter()
        .flat_map(|(_, query)| query.split('&'))
        .find_map(|pair| pair.strip_prefix("service_id="))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

/// A random wait below `nonce`, from the random bits of a version 4 UUID.
pub(crate) fn jitter(nonce: Duration) -> Duration {
    let millis = u64::try_from(nonce.as_millis()).unwrap_or(u64::MAX);
    if millis == 0 {
        return Duration::ZERO;
    }
    // The low 64 bits of the UUID are random but for two variant bits.
    let (_, random) = uuid::Uuid::new_v4().as_u64_pair();
    Duration::from_millis(random % millis)
}

/// Fragments of messages split over several frames, by `message_id`.
#[derive(Default)]
struct Fragments {
    partial: HashMap<String, Partial>,
}

struct Partial {
    slots: Vec<Option<Vec<u8>>>,
    updated: Instant,
}

impl Fragments {
    /// The whole payload once `frame`, whose payload is `payload`, completes its message;
    /// `None` while parts are missing, or when the frame's fragment headers are unusable.
    fn add(&mut self, frame: &Frame, payload: Vec<u8>, now: Instant) -> Option<Vec<u8>> {
        // A fragment header that is not a number makes the frame unusable, as a missing
        // `seq` or `message_id` does.
        let sum: usize = match frame.header("sum") {
            None => 1,
            Some(sum) => sum.parse().ok()?,
        };
        if sum <= 1 {
            return Some(payload);
        }
        let seq: usize = frame.header("seq")?.parse().ok()?;
        let id = frame.header("message_id")?;
        if seq >= sum || sum > MAX_FRAGMENTS {
            return None;
        }
        self.partial
            .retain(|_, partial| now.duration_since(partial.updated) < FRAGMENT_TTL);
        if !self.partial.contains_key(id) && self.partial.len() >= MAX_PARTIAL {
            return None;
        }
        let partial = self.partial.entry(id.to_string()).or_insert_with(|| Partial {
            slots: vec![None; sum],
            updated: now,
        });
        if partial.slots.len() != sum {
            partial.slots = vec![None; sum];
        }
        partial.slots[seq] = Some(payload);
        partial.updated = now;
        if partial.slots.iter().any(Option::is_none) {
            return None;
        }
        let complete = self.partial.remove(id)?;
        Some(complete.slots.into_iter().flatten().flatten().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fragment(id: &str, sum: usize, seq: usize, payload: &str) -> Frame {
        Frame {
            method: DATA,
            headers: vec![
                ("type".into(), "event".into()),
                ("message_id".into(), id.into()),
                ("sum".into(), sum.to_string()),
                ("seq".into(), seq.to_string()),
            ],
            payload: payload.as_bytes().to_vec(),
            ..Frame::default()
        }
    }

    /// Add `frame` with its payload taken out, as `deliver` does.
    fn add(fragments: &mut Fragments, mut frame: Frame, now: Instant) -> Option<Vec<u8>> {
        let payload = std::mem::take(&mut frame.payload);
        fragments.add(&frame, payload, now)
    }

    #[test]
    fn fragments_are_joined_in_seq_order_once_all_arrived() {
        let mut fragments = Fragments::default();
        let now = Instant::now();
        assert_eq!(add(&mut fragments, fragment("m", 3, 2, "c"), now), None);
        assert_eq!(add(&mut fragments, fragment("m", 3, 0, "a"), now), None);
        assert_eq!(
            add(&mut fragments, fragment("m", 3, 1, "b"), now),
            Some(b"abc".to_vec())
        );
        assert!(fragments.partial.is_empty());
    }

    #[test]
    fn a_frame_without_fragments_is_whole() {
        let frame = Frame {
            payload: b"x".to_vec(),
            ..Frame::default()
        };
        assert_eq!(
            add(&mut Fragments::default(), frame, Instant::now()),
            Some(b"x".to_vec())
        );
    }

    #[test]
    fn bad_fragment_headers_and_stale_parts_are_dropped() {
        let mut fragments = Fragments::default();
        let now = Instant::now();
        assert_eq!(
            add(&mut fragments, fragment("m", 2, 2, "x"), now),
            None,
            "seq beyond sum"
        );
        assert_eq!(add(&mut fragments, fragment("m", 2, 0, "a"), now), None);
        let later = now + FRAGMENT_TTL + Duration::from_millis(1);
        assert_eq!(
            add(&mut fragments, fragment("m", 2, 1, "b"), later),
            None,
            "the first part expired"
        );
    }

    #[test]
    fn a_frame_whose_sum_is_not_a_number_is_dropped() {
        let mut frame = fragment("m", 1, 0, "x");
        frame.headers.retain(|(key, _)| key != "sum");
        frame.headers.push(("sum".into(), "many".into()));
        assert_eq!(add(&mut Fragments::default(), frame, Instant::now()), None);
    }

    #[test]
    fn the_service_id_comes_from_the_address() {
        assert_eq!(service_id("wss://h/ws?device_id=d&service_id=33"), 33);
        assert_eq!(service_id("wss://h/ws?device_id=d"), 0);
    }

    #[test]
    fn an_acknowledgement_echoes_the_frame_with_biz_rt_and_a_success_code() {
        let frame = fragment("m", 1, 0, "{}");
        let ack = acknowledge(&frame);
        assert_eq!(ack.headers[..frame.headers.len()], frame.headers[..]);
        assert_eq!(ack.header("biz_rt"), Some("0"));
        assert_eq!(ack.payload, ACK_PAYLOAD);
        assert_eq!(
            (ack.method, ack.seq_id, ack.log_id),
            (frame.method, frame.seq_id, frame.log_id)
        );
    }

    #[test]
    fn missing_or_zero_settings_keep_the_current_ones() {
        let mut config = ClientConfig::default();
        config.update(&json!({"PingInterval": 0, "ReconnectInterval": 7}));
        assert_eq!(config.ping, DEFAULT_PING_INTERVAL);
        assert_eq!(config.reconnect, Some(Duration::from_secs(7)));
        config.update(&json!({"PingInterval": 1_000_000}));
        assert_eq!(config.ping, MAX_INTERVAL);
    }

    #[test]
    fn jitter_stays_below_the_nonce() {
        assert_eq!(jitter(Duration::ZERO), Duration::ZERO);
        for _ in 0..100 {
            assert!(jitter(Duration::from_millis(5)) < Duration::from_millis(5));
        }
    }
}
