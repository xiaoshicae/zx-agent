//! A local fake OpenAI-compatible server: one scripted reply per connection, every request
//! recorded. Real HTTP and SSE, no network access or API key.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

/// One request as the server received it.
#[derive(Clone, Debug)]
pub(crate) struct Captured {
    /// Request line and headers, lowercased.
    pub headers: String,
    pub body: Value,
}

impl Captured {
    /// The messages of the request, in order.
    fn messages(&self) -> &[Value] {
        self.body["messages"].as_array().map_or(&[], Vec::as_slice)
    }

    /// The texts of the user messages, in order.
    pub(crate) fn user_texts(&self) -> Vec<String> {
        self.messages()
            .iter()
            .filter(|message| message["role"] == "user")
            .map(|message| message["content"].as_str().unwrap().to_string())
            .collect()
    }

    /// The text of the leading system message.
    pub(crate) fn system_text(&self) -> String {
        self.messages()[0]["content"].as_str().unwrap().to_string()
    }

    /// The tool result messages, in order.
    pub(crate) fn tool_results(&self) -> Vec<Value> {
        self.messages()
            .iter()
            .filter(|message| message["role"] == "tool")
            .cloned()
            .collect()
    }
}

/// The scripted answer to one request.
pub(crate) enum Reply {
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
    pub(crate) fn text(text: &str) -> Self {
        Reply::Sse(vec![
            json!({"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": "stop"}]}),
            usage(),
        ])
    }

    /// A request for one `bash` call running `command`.
    pub(crate) fn bash(id: &str, command: &str) -> Self {
        Self::tool_call(id, "bash", &json!({"command": command}))
    }

    /// A request for one call of tool `name` with `arguments`.
    pub(crate) fn tool_call(id: &str, name: &str, arguments: &Value) -> Self {
        let call = json!({"index": 0, "id": id, "type": "function",
            "function": {"name": name, "arguments": arguments.to_string()}});
        Reply::Sse(vec![
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [call]}, "finish_reason": "tool_calls"}]}),
            usage(),
        ])
    }

    /// An answer cut off at the output token limit.
    pub(crate) fn truncated(text: &str) -> Self {
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
pub(crate) struct MockServer {
    /// Base URL to pass as `base_url`, ending in `/v1`.
    pub base_url: String,
    requests: Arc<Mutex<Vec<Captured>>>,
    received: Arc<Notify>,
}

impl MockServer {
    /// Start serving `replies`, one per connection in order.
    pub(crate) async fn start(replies: Vec<Reply>) -> Self {
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
    pub(crate) fn requests(&self) -> Vec<Captured> {
        self.requests.lock().unwrap().clone()
    }

    /// Wait until at least `count` requests have arrived.
    pub(crate) async fn wait_for_requests(&self, count: usize) {
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
    // A client that closes without sending has nothing to answer.
    let Some(request) = read_request(&mut stream).await else {
        return;
    };
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

/// One request, as the fake records it: `None` when the client closed the connection first.
async fn read_request(stream: &mut TcpStream) -> Option<Captured> {
    let request = super::http::read_request(stream).await?;
    let body = request.json();
    Some(Captured {
        headers: request.head_text().to_lowercase(),
        body,
    })
}
