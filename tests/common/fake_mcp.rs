//! A fake MCP server over Streamable HTTP, written from the specification rather than with
//! rmcp, so the client is checked against the protocol and not against itself. It speaks the
//! 2026-07-28 protocol (`server/discover`, no sessions) or the 2025-11-25 one (`initialize`
//! and `Mcp-Session-Id`). It lists one tool per page, so a client must follow `nextCursor`.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

use super::http::{read_request, response};

/// Only reached when an expected request never comes.
const WAIT: Duration = Duration::from_secs(10);
/// The session the 2025-11-25 server assigns.
pub const SESSION: &str = "session-1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Era {
    /// 2026-07-28: `server/discover`, and every request stands alone.
    Modern,
    /// 2025-11-25: the `initialize` handshake and a session.
    Legacy,
}

/// How the next call of a tool is answered. Without one, a call returns its arguments as text.
#[derive(Clone, Debug)]
pub enum Reply {
    /// The fields of the `tools/call` result, such as `{"content": [...], "isError": true}`.
    Result(Value),
    /// A JSON-RPC error.
    Error(i64, &'static str),
    /// No answer: the request stays open until the client closes it.
    Hang,
}

/// One request the server received.
#[derive(Clone, Debug)]
pub struct Received {
    /// `POST`, `GET` or `DELETE`.
    pub http_method: String,
    /// The JSON-RPC method; empty for `GET` and `DELETE`.
    pub method: String,
    pub params: Value,
    pub authorization: Option<String>,
    /// The `MCP-Protocol-Version` header.
    pub protocol_version: Option<String>,
    /// The `Mcp-Session-Id` header.
    pub session: Option<String>,
}

/// A tool as `tools/list` describes it, with one string argument `q`.
pub fn tool(name: &str, description: &str) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {"type": "object", "properties": {"q": {"type": "string"}}},
    })
}

struct State {
    era: Era,
    tools: Vec<Value>,
    received: Mutex<Vec<Received>>,
    replies: Mutex<HashMap<String, VecDeque<Reply>>>,
    /// Calls left unanswered that the client gave up on by closing the connection.
    abandoned: Mutex<usize>,
    changed: Notify,
}

pub struct FakeMcp {
    /// The MCP endpoint.
    pub url: String,
    state: Arc<State>,
}

impl FakeMcp {
    pub async fn start(era: Era, tools: Vec<Value>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let state = Arc::new(State {
            era,
            tools,
            received: Mutex::new(Vec::new()),
            replies: Mutex::new(HashMap::new()),
            abandoned: Mutex::new(0),
            changed: Notify::new(),
        });
        let serving = state.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve(stream, serving.clone()));
            }
        });
        Self { url, state }
    }

    /// Answer the next calls of `tool` this way, in order, before answering normally.
    pub fn reply(&self, tool: &str, replies: Vec<Reply>) {
        self.state
            .replies
            .lock()
            .unwrap()
            .entry(tool.to_string())
            .or_default()
            .extend(replies);
    }

    pub fn received(&self) -> Vec<Received> {
        self.state.received.lock().unwrap().clone()
    }

    /// The requests with JSON-RPC method `method`.
    pub fn requests(&self, method: &str) -> Vec<Received> {
        self.received().into_iter().filter(|r| r.method == method).collect()
    }

    /// Wait until `method` was requested at least `count` times.
    pub async fn wait_for(&self, method: &str, count: usize) -> Vec<Received> {
        tokio::time::timeout(WAIT, async {
            loop {
                let changed = self.state.changed.notified();
                let requests = self.requests(method);
                if requests.len() >= count {
                    return requests;
                }
                changed.await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{method} was not requested {count} times in time"))
    }

    /// Wait until the client closed `count` unanswered calls.
    pub async fn wait_abandoned(&self, count: usize) {
        tokio::time::timeout(WAIT, async {
            loop {
                let changed = self.state.changed.notified();
                if *self.state.abandoned.lock().unwrap() >= count {
                    return;
                }
                changed.await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("the client did not give up {count} calls in time"));
    }
}

async fn serve(mut stream: TcpStream, state: Arc<State>) {
    let Some(request) = read_request(&mut stream).await else {
        return;
    };
    let body = request.json();
    state.received.lock().unwrap().push(Received {
        http_method: request.method.clone(),
        method: body["method"].as_str().unwrap_or_default().to_string(),
        params: body["params"].clone(),
        authorization: request.header("authorization").map(String::from),
        protocol_version: request.header("mcp-protocol-version").map(String::from),
        session: request.header("mcp-session-id").map(String::from),
    });
    state.changed.notify_waiters();
    let answer = match request.method.as_str() {
        // No server-initiated stream: the spec lets a server refuse it.
        "GET" => Some(response("405 Method Not Allowed", &[], "")),
        "DELETE" => Some(response("200 OK", &[], "")),
        _ => answer(&state, &body),
    };
    match answer {
        Some(text) => {
            stream.write_all(text.as_bytes()).await.ok();
            stream.shutdown().await.ok();
        }
        None => {
            // Hold the call until the client closes the connection.
            let mut buf = [0u8; 256];
            while let Ok(n) = stream.read(&mut buf).await {
                if n == 0 {
                    break;
                }
            }
            *state.abandoned.lock().unwrap() += 1;
            state.changed.notify_waiters();
        }
    }
}

/// The HTTP response to a JSON-RPC message; `None` leaves the request unanswered.
fn answer(state: &State, body: &Value) -> Option<String> {
    let method = body["method"].as_str().unwrap_or_default();
    let id = body["id"].clone();
    if id.is_null() {
        // Notifications, such as `notifications/initialized` and `notifications/cancelled`.
        return Some(response("202 Accepted", &[], ""));
    }
    let modern = state.era == Era::Modern;
    let result = match (method, modern) {
        // A 2025-11-25 server has no such method and wants a session first.
        ("server/discover", false) => {
            return Some(response("400 Bad Request", &[], "Bad Request: no valid session ID"));
        }
        ("server/discover", true) => json!({
            "resultType": "complete",
            "supportedVersions": ["2026-07-28"],
            "capabilities": {"tools": {}},
            "ttlMs": 0,
            "cacheScope": "private",
            "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "fake", "version": "1"}},
        }),
        ("initialize", false) => {
            let result = json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fake", "version": "1"},
            });
            let text = json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
            return Some(response("200 OK", &[("mcp-session-id", SESSION.to_string())], &text));
        }
        ("tools/list", _) => {
            let index: usize = body["params"]["cursor"]
                .as_str()
                .map_or(0, |cursor| cursor.parse().unwrap());
            let mut result = json!({"tools": state.tools.get(index).into_iter().collect::<Vec<_>>()});
            if index + 1 < state.tools.len() {
                result["nextCursor"] = json!((index + 1).to_string());
            }
            if modern {
                result["resultType"] = json!("complete");
                result["ttlMs"] = json!(0);
                result["cacheScope"] = json!("private");
            }
            result
        }
        ("tools/call", _) => {
            let name = body["params"]["name"].as_str().unwrap_or_default().to_string();
            let reply = state
                .replies
                .lock()
                .unwrap()
                .get_mut(&name)
                .and_then(VecDeque::pop_front);
            let mut result = match reply {
                Some(Reply::Hang) => return None,
                Some(Reply::Error(code, message)) => {
                    let error = json!({"code": code, "message": message});
                    return Some(response(
                        "200 OK",
                        &[],
                        &json!({"jsonrpc": "2.0", "id": id, "error": error}).to_string(),
                    ));
                }
                Some(Reply::Result(fields)) => fields,
                None => {
                    json!({"content": [{"type": "text", "text": format!("{name}: {}", body["params"]["arguments"])}]})
                }
            };
            if modern {
                result["resultType"] = json!("complete");
            }
            result
        }
        _ => {
            let error = json!({"code": -32601, "message": format!("method not found: {method}")});
            return Some(response(
                "200 OK",
                &[],
                &json!({"jsonrpc": "2.0", "id": id, "error": error}).to_string(),
            ));
        }
    };
    Some(response(
        "200 OK",
        &[],
        &json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string(),
    ))
}
