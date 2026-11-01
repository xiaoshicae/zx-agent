//! A fake Feishu open platform: the token, long-connection endpoint and message APIs, and
//! long connections the test drives frame by frame. Frames are encoded here independently
//! of the channel's own codec, so that a mistake mirrored in both cannot go unnoticed.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

use super::http::{read_request, response};

/// Only reached when an expected frame or call never comes.
const WAIT: Duration = Duration::from_secs(10);

/// One API call.
#[derive(Clone, Debug)]
pub struct Call {
    /// Which API: token, endpoint, create, reply, edit or patch.
    pub route: &'static str,
    /// Path and query.
    pub path: String,
    pub authorization: String,
    pub locale: Option<String>,
    pub body: Value,
}

/// A scripted answer to one call of a route.
#[derive(Clone, Debug)]
pub enum Answer {
    /// Status 200 with this non-zero code.
    Code(i64),
    /// This HTTP status, with this code in the body.
    Status(u16, i64),
    /// Status 429 with the gateway's code and this reset time.
    RateLimited { reset_secs: u64 },
}

pub struct FakeFeishu {
    /// Pass as the channel's API base.
    pub api_base: String,
    /// Pass as the channel's token URL.
    pub token_url: String,
    calls: Arc<Mutex<Vec<Call>>>,
    called: Arc<Notify>,
    scripted: Arc<Mutex<HashMap<&'static str, VecDeque<Answer>>>>,
    sockets: mpsc::UnboundedReceiver<Socket>,
}

impl FakeFeishu {
    /// Start with this ping interval in the connection settings it hands out.
    pub async fn start(ping_secs: u64) -> Self {
        let api = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!(
            "ws://{}/ws?device_id=d1&service_id=33",
            socket_listener.local_addr().unwrap()
        );
        let api_base = format!("http://{}", api.local_addr().unwrap());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let called = Arc::new(Notify::new());
        let scripted: Arc<Mutex<HashMap<&'static str, VecDeque<Answer>>>> = Arc::default();
        let (log, notify, script) = (calls.clone(), called.clone(), scripted.clone());
        tokio::spawn(async move {
            let posted = Arc::new(Mutex::new(0u64));
            loop {
                let (stream, _) = api.accept().await.unwrap();
                let settings = Settings {
                    address: address.clone(),
                    ping_secs,
                };
                tokio::spawn(answer(
                    stream,
                    settings,
                    log.clone(),
                    notify.clone(),
                    script.clone(),
                    posted.clone(),
                ));
            }
        });
        let (sockets_to, sockets) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let (stream, _) = socket_listener.accept().await.unwrap();
                let stream = tokio_tungstenite::accept_async(stream).await.unwrap();
                if sockets_to.send(Socket { stream }).is_err() {
                    return;
                }
            }
        });
        Self {
            token_url: format!("{api_base}/oauth/v3/token"),
            api_base,
            calls,
            called,
            scripted,
            sockets,
        }
    }

    /// Answer the next calls of `route` this way, in order, before answering normally.
    pub fn script(&self, route: &'static str, answers: Vec<Answer>) {
        self.scripted.lock().unwrap().entry(route).or_default().extend(answers);
    }

    /// The next long connection.
    pub async fn socket(&mut self) -> Socket {
        tokio::time::timeout(WAIT, self.sockets.recv())
            .await
            .expect("the channel did not connect in time")
            .expect("the listener is alive")
    }

    pub fn calls(&self, route: &str) -> Vec<Call> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.route == route)
            .cloned()
            .collect()
    }

    /// Wait until `route` was called at least `count` times, and return its calls.
    pub async fn wait_for(&self, route: &str, count: usize) -> Vec<Call> {
        tokio::time::timeout(WAIT, async {
            loop {
                let notified = self.called.notified();
                let calls = self.calls(route);
                if calls.len() >= count {
                    return calls;
                }
                notified.await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{route} was not called {count} times in time"))
    }
}

struct Settings {
    address: String,
    ping_secs: u64,
}

/// One long connection, from the server's side.
pub struct Socket {
    stream: WebSocketStream<TcpStream>,
}

impl Socket {
    pub async fn send(&mut self, frame: &TestFrame) {
        self.stream.send(Message::binary(frame.encode())).await.unwrap();
    }

    /// The next frame from the client.
    pub async fn receive(&mut self) -> TestFrame {
        loop {
            let message = tokio::time::timeout(WAIT, self.stream.next())
                .await
                .expect("the client sent nothing in time")
                .expect("the connection is open")
                .unwrap();
            match message {
                Message::Binary(bytes) => return TestFrame::decode(&bytes),
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
                other => panic!("expected a binary frame, got {other:?}"),
            }
        }
    }

    /// The next data frame from the client, skipping its pings.
    pub async fn receive_data(&mut self) -> TestFrame {
        loop {
            let frame = self.receive().await;
            if frame.method == 1 {
                return frame;
            }
        }
    }

    pub async fn close(mut self) {
        self.stream.close(None).await.ok();
    }

    /// Whether the client closed the connection, waiting for it.
    pub async fn closed_by_client(&mut self) -> bool {
        loop {
            match tokio::time::timeout(WAIT, self.stream.next()).await {
                Ok(None | Some(Err(_)) | Some(Ok(Message::Close(_)))) => return true,
                Ok(Some(Ok(_))) => {}
                Err(_) => return false,
            }
        }
    }
}

/// A frame as the test sees it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TestFrame {
    pub seq_id: u64,
    pub log_id: u64,
    pub service: i32,
    pub method: i32,
    pub headers: Vec<(String, String)>,
    pub payload_encoding: String,
    pub payload_type: String,
    pub payload: Vec<u8>,
    pub log_id_new: String,
}

impl TestFrame {
    /// A data frame of type `event` carrying `payload`, as fragment `seq` of `sum`.
    pub fn event(message_id: &str, sum: usize, seq: usize, payload: &[u8]) -> Self {
        TestFrame {
            seq_id: 7,
            log_id: 8,
            service: 33,
            method: 1,
            headers: vec![
                ("type".into(), "event".into()),
                ("message_id".into(), message_id.into()),
                ("sum".into(), sum.to_string()),
                ("seq".into(), seq.to_string()),
                ("trace_id".into(), "t1".into()),
            ],
            log_id_new: "n1".into(),
            payload: payload.to_vec(),
            ..TestFrame::default()
        }
    }

    pub fn header(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    fn encode(&self) -> Vec<u8> {
        fn varint(out: &mut Vec<u8>, mut value: u64) {
            loop {
                let byte = (value & 0x7f) as u8;
                value >>= 7;
                if value == 0 {
                    out.push(byte);
                    return;
                }
                out.push(byte | 0x80);
            }
        }
        fn field(out: &mut Vec<u8>, number: u64, bytes: &[u8]) {
            varint(out, number << 3 | 2);
            varint(out, bytes.len() as u64);
            out.extend_from_slice(bytes);
        }
        let mut out = Vec::new();
        for (number, value) in [
            (1, self.seq_id),
            (2, self.log_id),
            (3, self.service as i64 as u64),
            (4, self.method as i64 as u64),
        ] {
            varint(&mut out, number << 3);
            varint(&mut out, value);
        }
        for (key, value) in &self.headers {
            let mut header = Vec::new();
            field(&mut header, 1, key.as_bytes());
            field(&mut header, 2, value.as_bytes());
            field(&mut out, 5, &header);
        }
        field(&mut out, 6, self.payload_encoding.as_bytes());
        field(&mut out, 7, self.payload_type.as_bytes());
        field(&mut out, 8, &self.payload);
        field(&mut out, 9, self.log_id_new.as_bytes());
        out
    }

    fn decode(bytes: &[u8]) -> Self {
        fn varint(bytes: &mut &[u8]) -> u64 {
            let mut value = 0;
            let mut shift = 0;
            loop {
                let byte = bytes[0];
                *bytes = &bytes[1..];
                value |= u64::from(byte & 0x7f) << shift;
                if byte & 0x80 == 0 {
                    return value;
                }
                shift += 7;
            }
        }
        fn chunk<'a>(bytes: &mut &'a [u8]) -> &'a [u8] {
            let length = varint(bytes) as usize;
            let (field, rest) = bytes.split_at(length);
            *bytes = rest;
            field
        }
        let text = |bytes: &[u8]| String::from_utf8(bytes.to_vec()).unwrap();
        let mut frame = TestFrame::default();
        let mut bytes = bytes;
        while !bytes.is_empty() {
            match varint(&mut bytes) {
                0x08 => frame.seq_id = varint(&mut bytes),
                0x10 => frame.log_id = varint(&mut bytes),
                0x18 => frame.service = varint(&mut bytes) as i32,
                0x20 => frame.method = varint(&mut bytes) as i32,
                0x2a => {
                    let mut header = chunk(&mut bytes);
                    let (mut key, mut value) = (String::new(), String::new());
                    while !header.is_empty() {
                        match varint(&mut header) {
                            0x0a => key = text(chunk(&mut header)),
                            0x12 => value = text(chunk(&mut header)),
                            other => panic!("unexpected header field {other:#x}"),
                        }
                    }
                    frame.headers.push((key, value));
                }
                0x32 => frame.payload_encoding = text(chunk(&mut bytes)),
                0x3a => frame.payload_type = text(chunk(&mut bytes)),
                0x42 => frame.payload = chunk(&mut bytes).to_vec(),
                0x4a => frame.log_id_new = text(chunk(&mut bytes)),
                other => panic!("unexpected frame field {other:#x}"),
            }
        }
        frame
    }
}

fn route(method: &str, path: &str) -> &'static str {
    match (method, path) {
        ("POST", "/oauth/v3/token") => "token",
        ("POST", "/callback/ws/endpoint") => "endpoint",
        ("POST", path) if path.starts_with("/open-apis/im/v1/messages?") => "create",
        ("POST", path) if path.ends_with("/reply") => "reply",
        ("PUT", _) => "edit",
        ("PATCH", _) => "patch",
        _ => "unknown",
    }
}

async fn answer(
    mut stream: TcpStream,
    settings: Settings,
    log: Arc<Mutex<Vec<Call>>>,
    called: Arc<Notify>,
    scripted: Arc<Mutex<HashMap<&'static str, VecDeque<Answer>>>>,
    posted: Arc<Mutex<u64>>,
) {
    let Some(request) = read_request(&mut stream).await else {
        return;
    };
    let route = route(&request.method, &request.path);
    let call = Call {
        route,
        path: request.path.clone(),
        authorization: request.header("authorization").unwrap_or_default().to_string(),
        locale: request.header("locale").map(String::from),
        body: request.json(),
    };
    let scripted = scripted.lock().unwrap().get_mut(route).and_then(VecDeque::pop_front);
    let reply = match scripted {
        Some(Answer::Code(code)) => response("200 OK", &[], &json!({"code": code, "msg": "refused"}).to_string()),
        Some(Answer::Status(status, code)) => response(
            &format!("{status} Refused"),
            &[],
            &json!({"code": code, "msg": "refused", "error_description": "refused"}).to_string(),
        ),
        Some(Answer::RateLimited { reset_secs }) => response(
            "429 Too Many Requests",
            &[("x-ogw-ratelimit-reset", reset_secs.to_string())],
            &json!({"code": 99991400, "msg": "request trigger frequency limit"}).to_string(),
        ),
        None => response("200 OK", &[], &normal(route, &settings, &posted).to_string()),
    };
    log.lock().unwrap().push(call);
    called.notify_waiters();
    stream.write_all(reply.as_bytes()).await.unwrap();
    stream.shutdown().await.ok();
}

/// What Feishu answers when the call succeeds.
fn normal(route: &str, settings: &Settings, posted: &Mutex<u64>) -> Value {
    match route {
        "token" => json!({"code": 0, "access_token": "t-1", "expires_in": 7200, "token_type": "Bearer"}),
        "endpoint" => json!({"code": 0, "msg": "ok", "data": {
            "URL": settings.address,
            "ClientConfig": {"ReconnectCount": -1, "ReconnectInterval": 1, "ReconnectNonce": 0, "PingInterval": settings.ping_secs},
        }}),
        "create" | "reply" => {
            let mut count = posted.lock().unwrap();
            *count += 1;
            json!({"code": 0, "msg": "success", "data": {"message_id": format!("om_out{count}")}})
        }
        "edit" | "patch" => json!({"code": 0, "msg": "success", "data": {}}),
        other => json!({"code": 404, "msg": format!("unknown route {other}")}),
    }
}
