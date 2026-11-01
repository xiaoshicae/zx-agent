//! A local fake OpenAI-compatible server: one scripted reply per connection, every request
//! recorded. Real HTTP and SSE, no network access or API key.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

/// One request as the server received it.
#[derive(Clone, Debug)]
pub struct Captured {
    /// Request line and headers, lowercased.
    pub headers: String,
    pub body: Value,
}

/// The scripted answer to one request.
pub enum Reply {
    /// Stream these chunks, then `[DONE]`.
    Sse(Vec<Value>),
    /// Build SSE chunks from the request body, e.g. to read a newly generated output ID.
    Dynamic(fn(&Value) -> Vec<Value>),
    /// Stream these chunks, then keep the connection open without sending anything more.
    Stall(Vec<Value>),
    /// Send partial output, then finish only after the test releases the gate.
    Gate { chunks: Vec<Value>, release: Arc<Notify> },
    /// Answer with an HTTP error status and a JSON body.
    Status(u16, &'static str),
    /// A complete HTTP response, for custom headers and invalid/oversized payloads.
    Raw(String),
}

impl Reply {
    /// A plain answer that ends normally.
    pub fn text(text: &str) -> Self {
        Reply::Sse(vec![
            json!({"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": "stop"}]}),
            usage(),
        ])
    }

    /// A request for one `bash` call running `command`.
    pub fn bash(id: &str, command: &str) -> Self {
        let call = json!({"index": 0, "id": id, "type": "function",
            "function": {"name": "bash", "arguments": json!({"command": command}).to_string()}});
        Reply::Sse(vec![
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [call]}, "finish_reason": "tool_calls"}]}),
            usage(),
        ])
    }

    /// An answer cut off at the output token limit.
    pub fn truncated(text: &str) -> Self {
        Reply::Sse(vec![
            json!({"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": "length"}]}),
            usage(),
        ])
    }
}

fn usage() -> Value {
    json!({"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12}})
}

/// A running fake server. It stops answering after the scripted replies are used up.
pub struct MockServer {
    /// Base URL to pass as `base_url`, ending in `/v1`.
    pub base_url: String,
    requests: Arc<Mutex<Vec<Captured>>>,
    received: Arc<Notify>,
}

impl MockServer {
    /// Start serving `replies`, one per connection in order.
    pub async fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let received = Arc::new(Notify::new());
        let (log, notify) = (requests.clone(), received.clone());
        tokio::spawn(async move {
            for reply in replies {
                let (stream, _) = listener.accept().await.unwrap();
                // One task per connection, so a stalled reply does not block the next one.
                tokio::spawn(answer(stream, reply, log.clone(), notify.clone()));
            }
        });
        Self {
            base_url,
            requests,
            received,
        }
    }

    /// Requests received so far, in order.
    pub fn requests(&self) -> Vec<Captured> {
        self.requests.lock().unwrap().clone()
    }

    /// Wait until at least `count` requests have arrived.
    pub async fn wait_for_requests(&self, count: usize) {
        loop {
            let notified = self.received.notified();
            if self.requests.lock().unwrap().len() >= count {
                return;
            }
            notified.await;
        }
    }
}

async fn answer(mut stream: TcpStream, reply: Reply, log: Arc<Mutex<Vec<Captured>>>, received: Arc<Notify>) {
    let request = read_request(&mut stream).await;
    let reply = match reply {
        Reply::Dynamic(build) => Reply::Sse(build(&request.body)),
        reply => reply,
    };
    log.lock().unwrap().push(request);
    received.notify_one();
    let sse_head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
    match reply {
        Reply::Sse(chunks) => {
            let response = format!("{sse_head}{}data: [DONE]\n\n", sse_events(&chunks));
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
        }
        Reply::Stall(chunks) => {
            let response = format!("{sse_head}{}", sse_events(&chunks));
            stream.write_all(response.as_bytes()).await.unwrap();
            // Hold the connection open; the client gives up first.
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
        Reply::Gate { chunks, release } => {
            let response = format!("{sse_head}{}", sse_events(&chunks));
            stream.write_all(response.as_bytes()).await.unwrap();
            release.notified().await;
            let finish = json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]});
            let response = format!("{}data: [DONE]\n\n", sse_events(&[finish]));
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
        }
        Reply::Status(code, body) => {
            let response = format!(
                "HTTP/1.1 {code} Error\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
        }
        Reply::Raw(response) => {
            // Limit/cancellation tests intentionally close the connection early.
            let _ = stream.write_all(response.as_bytes()).await;
        }
        Reply::Dynamic(_) => unreachable!("dynamic replies are resolved before recording the request"),
    }
}

fn sse_events(chunks: &[Value]) -> String {
    chunks.iter().map(|chunk| format!("data: {chunk}\n\n")).collect()
}

async fn read_request(stream: &mut TcpStream) -> Captured {
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut buf).await.unwrap();
        data.extend_from_slice(&buf[..n]);
        if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let headers = String::from_utf8_lossy(&data[..header_end]).to_lowercase();
    let length: usize = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .map(|value| value.trim().parse().unwrap())
        .unwrap_or(0);
    while data.len() < header_end + length {
        let n = stream.read(&mut buf).await.unwrap();
        data.extend_from_slice(&buf[..n]);
    }
    Captured {
        headers,
        body: serde_json::from_slice(&data[header_end..header_end + length]).unwrap(),
    }
}
