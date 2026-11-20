//! A fake Slack: the Web API methods the Slack channel calls, the private URLs of the files
//! it keeps, and Socket Mode connections the test drives frame by frame.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

use crate::WAIT;
use crate::http::{read_request, response, response_bytes};

/// Where the fake keeps files, as Slack's private URLs name them.
const FILES: &str = "/files-pri/T1-F1/";

/// The bot token the fake gives files to, the one the Slack tests use.
pub const BOT_TOKEN: &str = "xoxb-test";

/// One Web API call, or one fetch of a kept file.
#[derive(Clone, Debug)]
pub struct ApiCall {
    /// The Web API method, such as `chat.postMessage`, or the path of a file fetched, such as
    /// `/files-pri/T1-F1/shot.png`.
    pub method: String,
    /// The `Authorization` header; empty without one.
    pub authorization: String,
    /// The arguments in the query, decoded, in order.
    pub args: Vec<(String, String)>,
    /// The JSON body; `Null` when the body was empty.
    pub body: Value,
}

impl ApiCall {
    /// The query argument `name`.
    pub fn arg(&self, name: &str) -> Option<&str> {
        self.args
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// A scripted answer to one call of a method.
#[derive(Clone, Debug)]
pub enum Answer {
    /// `{"ok": false, "error": ...}` with status 200, as Slack answers method errors.
    Error(&'static str),
    /// Status 429 with this `Retry-After`.
    RateLimited {
        /// The `Retry-After` header, in seconds.
        retry_after_secs: u64,
    },
    /// Status 200 with this body, for answers the normal ones do not cover.
    Json(Value),
}

/// A running fake Slack: its Web API and its Socket Mode endpoint, on local ports.
pub struct FakeSlack {
    /// Pass as the channel's Web API base.
    pub api_base: String,
    calls: Arc<Mutex<Vec<ApiCall>>>,
    called: Arc<Notify>,
    scripted: Arc<Mutex<HashMap<String, VecDeque<Answer>>>>,
    /// The files kept, by path: their type and bytes.
    files: Files,
    sockets: mpsc::UnboundedReceiver<Socket>,
}

/// Files by path, with their type and bytes.
type Files = Arc<Mutex<HashMap<String, (String, Vec<u8>)>>>;

impl FakeSlack {
    /// Start the Web API and the Socket Mode endpoint that `apps.connections.open` names.
    pub async fn start() -> Self {
        let api = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket_url = format!("ws://{}/link", socket_listener.local_addr().unwrap());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let called = Arc::new(Notify::new());
        let scripted: Arc<Mutex<HashMap<String, VecDeque<Answer>>>> = Arc::default();
        let files = Files::default();
        let (log, notify, script, kept) = (calls.clone(), called.clone(), scripted.clone(), files.clone());
        let api_base = format!("http://{}/api", api.local_addr().unwrap());
        tokio::spawn(async move {
            let posted = Arc::new(Mutex::new(0u64));
            loop {
                let (stream, _) = api.accept().await.unwrap();
                let fake = Answering {
                    socket_url: socket_url.clone(),
                    log: log.clone(),
                    called: notify.clone(),
                    scripted: script.clone(),
                    posted: posted.clone(),
                    files: kept.clone(),
                };
                tokio::spawn(fake.answer(stream));
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
            files,
            sockets,
        }
    }

    /// Keep a file named `name` of `content_type` with `bytes`, and return its private URL,
    /// which answers only with the bot token.
    pub fn file(&self, name: &str, content_type: &str, bytes: Vec<u8>) -> String {
        let path = format!("{FILES}{name}");
        self.files
            .lock()
            .unwrap()
            .insert(path.clone(), (content_type.to_string(), bytes));
        format!("{}{path}", self.api_base.trim_end_matches("/api"))
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

    /// The calls of `method` so far, in order.
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
    /// Send `frame` to the client as one text message.
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
        crate::http::closed_by_client(&mut self.stream).await
    }
}

/// What answers one connection to the Web API.
struct Answering {
    socket_url: String,
    log: Arc<Mutex<Vec<ApiCall>>>,
    called: Arc<Notify>,
    scripted: Arc<Mutex<HashMap<String, VecDeque<Answer>>>>,
    posted: Arc<Mutex<u64>>,
    files: Files,
}

impl Answering {
    async fn answer(self, mut stream: TcpStream) {
        let Some(request) = read_request(&mut stream).await else {
            return;
        };
        let (path, query) = request.path.split_once('?').unwrap_or((&request.path, ""));
        let call = ApiCall {
            method: path.trim_start_matches("/api/").to_string(),
            authorization: request.header("authorization").unwrap_or_default().to_string(),
            args: query_args(query),
            body: if path.starts_with(FILES) {
                Value::Null
            } else {
                request.json()
            },
        };
        let scripted = self
            .scripted
            .lock()
            .unwrap()
            .get_mut(&call.method)
            .and_then(VecDeque::pop_front);
        let response = match scripted {
            Some(Answer::RateLimited { retry_after_secs }) => response(
                "429 Too Many Requests",
                &[("retry-after", retry_after_secs.to_string())],
                "",
            )
            .into_bytes(),
            Some(Answer::Error(error)) => {
                response("200 OK", &[], &json!({"ok": false, "error": error}).to_string()).into_bytes()
            }
            Some(Answer::Json(body)) => response("200 OK", &[], &body.to_string()).into_bytes(),
            None if path.starts_with(FILES) => self.file(path, &call.authorization),
            None => response(
                "200 OK",
                &[],
                &normal(&call, &self.socket_url, &self.posted).to_string(),
            )
            .into_bytes(),
        };
        self.log.lock().unwrap().push(call);
        self.called.notify_waiters();
        stream.write_all(&response).await.unwrap();
        stream.shutdown().await.ok();
    }

    /// The file kept at `path`, for [`BOT_TOKEN`]; as Slack does, a sign-in page for another.
    fn file(&self, path: &str, authorization: &str) -> Vec<u8> {
        if authorization != format!("Bearer {BOT_TOKEN}") {
            return response_bytes("200 OK", "text/html; charset=utf-8", b"<html>sign in</html>");
        }
        match self.files.lock().unwrap().get(path) {
            Some((content_type, bytes)) => response_bytes("200 OK", content_type, bytes),
            None => response_bytes("404 Not Found", "text/plain", b"not found"),
        }
    }
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
        "conversations.replies" => json!({"ok": true, "messages": []}),
        "users.info" => json!({"ok": true, "user": {"id": call.arg("user"), "profile": {"display_name": ""}}}),
        "reactions.add" => json!({"ok": true}),
        other => json!({"ok": false, "error": format!("unknown_method {other}")}),
    }
}

/// `a=1&b=x%20y` as pairs, `+` and `%XX` decoded.
fn query_args(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(key), decode(value))
        })
        .collect()
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap();
                out.push(u8::from_str_radix(hex, 16).unwrap());
                i += 2;
            }
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8(out).unwrap()
}
