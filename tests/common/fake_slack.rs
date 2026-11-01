//! A fake Slack: the Web API methods the Slack channel calls, and Socket Mode connections the
//! test drives frame by frame.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

/// Only reached when an expected frame or call never comes.
const WAIT: Duration = Duration::from_secs(10);

/// One Web API call.
#[derive(Clone, Debug)]
pub struct ApiCall {
    pub method: String,
    pub authorization: String,
    /// The JSON body; `Null` when the body was empty.
    pub body: Value,
}

/// A scripted answer to one call of a method.
#[derive(Clone, Debug)]
pub enum Answer {
    /// `{"ok": false, "error": ...}` with status 200, as Slack answers method errors.
    Error(&'static str),
    /// Status 429 with this `Retry-After`.
    RateLimited { retry_after_secs: u64 },
}

pub struct FakeSlack {
    /// Pass as the channel's Web API base.
    pub api_base: String,
    calls: Arc<Mutex<Vec<ApiCall>>>,
    called: Arc<Notify>,
    scripted: Arc<Mutex<HashMap<String, VecDeque<Answer>>>>,
    sockets: mpsc::UnboundedReceiver<Socket>,
}

impl FakeSlack {
    pub async fn start() -> Self {
        let api = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket_url = format!("ws://{}/link", socket_listener.local_addr().unwrap());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let called = Arc::new(Notify::new());
        let scripted: Arc<Mutex<HashMap<String, VecDeque<Answer>>>> = Arc::default();
        let (log, notify, script) = (calls.clone(), called.clone(), scripted.clone());
        let api_base = format!("http://{}/api", api.local_addr().unwrap());
        tokio::spawn(async move {
            let posted = Arc::new(Mutex::new(0u64));
            loop {
                let (stream, _) = api.accept().await.unwrap();
                tokio::spawn(answer(
                    stream,
                    socket_url.clone(),
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
            api_base,
            calls,
            called,
            scripted,
            sockets,
        }
    }

    /// Answer the next calls of `method` this way, in order, before answering normally.
    pub fn script(&self, method: &str, answers: Vec<Answer>) {
        self.scripted
            .lock()
            .unwrap()
            .entry(method.to_string())
            .or_default()
            .extend(answers);
    }

    /// The next Socket Mode connection.
    pub async fn socket(&mut self) -> Socket {
        tokio::time::timeout(WAIT, self.sockets.recv())
            .await
            .expect("the channel did not connect in time")
            .expect("the listener is alive")
    }

    pub fn calls(&self, method: &str) -> Vec<ApiCall> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.method == method)
            .cloned()
            .collect()
    }

    /// Wait until `method` was called at least `count` times, and return its calls.
    pub async fn wait_for(&self, method: &str, count: usize) -> Vec<ApiCall> {
        tokio::time::timeout(WAIT, async {
            loop {
                let notified = self.called.notified();
                let calls = self.calls(method);
                if calls.len() >= count {
                    return calls;
                }
                notified.await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{method} was not called {count} times in time"))
    }
}

/// One Socket Mode connection, from the server's side.
pub struct Socket {
    stream: WebSocketStream<TcpStream>,
}

impl Socket {
    pub async fn send(&mut self, frame: Value) {
        self.stream.send(Message::text(frame.to_string())).await.unwrap();
    }

    /// The next text frame from the client, as JSON. Control frames are skipped.
    pub async fn receive(&mut self) -> Value {
        loop {
            let frame = tokio::time::timeout(WAIT, self.stream.next())
                .await
                .expect("the client sent nothing in time")
                .expect("the connection is open")
                .unwrap();
            match frame {
                Message::Text(text) => return serde_json::from_str(&text).unwrap(),
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
                other => panic!("expected a text frame, got {other:?}"),
            }
        }
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

async fn answer(
    mut stream: TcpStream,
    socket_url: String,
    log: Arc<Mutex<Vec<ApiCall>>>,
    called: Arc<Notify>,
    scripted: Arc<Mutex<HashMap<String, VecDeque<Answer>>>>,
    posted: Arc<Mutex<u64>>,
) {
    let Some(call) = read_call(&mut stream).await else {
        return;
    };
    let scripted = scripted
        .lock()
        .unwrap()
        .get_mut(&call.method)
        .and_then(VecDeque::pop_front);
    let response = match scripted {
        Some(Answer::RateLimited { retry_after_secs }) => {
            format!(
                "HTTP/1.1 429 Too Many Requests\r\nretry-after: {retry_after_secs}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            )
        }
        Some(Answer::Error(error)) => json_response(&json!({"ok": false, "error": error})),
        None => json_response(&normal(&call, &socket_url, &posted)),
    };
    log.lock().unwrap().push(call);
    called.notify_waiters();
    stream.write_all(response.as_bytes()).await.unwrap();
    stream.shutdown().await.ok();
}

/// What Slack answers when the call succeeds.
fn normal(call: &ApiCall, socket_url: &str, posted: &Mutex<u64>) -> Value {
    match call.method.as_str() {
        "apps.connections.open" => json!({"ok": true, "url": socket_url}),
        "auth.test" => json!({"ok": true, "user_id": "UBOT", "bot_id": "BBOT", "user": "zx-agent"}),
        "chat.postMessage" => {
            let mut count = posted.lock().unwrap();
            *count += 1;
            json!({"ok": true, "channel": call.body["channel"], "ts": format!("1700000000.{:06}", *count)})
        }
        "chat.update" => json!({"ok": true, "channel": call.body["channel"], "ts": call.body["ts"]}),
        other => json!({"ok": false, "error": format!("unknown_method {other}")}),
    }
}

fn json_response(body: &Value) -> String {
    let body = body.to_string();
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

async fn read_call(stream: &mut TcpStream) -> Option<ApiCall> {
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        data.extend_from_slice(&buf[..n]);
        if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&data[..header_end]).into_owned();
    let path = head.lines().next()?.split_whitespace().nth(1)?.to_string();
    let header = |name: &str| {
        head.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim().eq_ignore_ascii_case(name).then(|| value.trim().to_string())
        })
    };
    let length: usize = header("content-length").map_or(0, |value| value.parse().unwrap());
    while data.len() < header_end + length {
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        data.extend_from_slice(&buf[..n]);
    }
    let body = &data[header_end..header_end + length];
    Some(ApiCall {
        method: path.trim_start_matches("/api/").to_string(),
        authorization: header("authorization").unwrap_or_default(),
        body: if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(body).unwrap()
        },
    })
}
