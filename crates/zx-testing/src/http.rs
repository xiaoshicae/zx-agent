//! What the fake servers share: one HTTP/1.1 request per connection, read and answered; a
//! listener that never answers; and whether a client closed its WebSocket, waited for as
//! long as [`WAIT`].

use futures::StreamExt;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

use crate::WAIT;

/// Whether the client closed the WebSocket `stream`, waiting up to [`WAIT`] for it.
pub(crate) async fn closed_by_client(stream: &mut WebSocketStream<TcpStream>) -> bool {
    loop {
        match tokio::time::timeout(WAIT, stream.next()).await {
            Ok(None | Some(Err(_) | Ok(Message::Close(_)))) => return true,
            Ok(Some(Ok(_))) => {}
            Err(_) => return false,
        }
    }
}

/// One request as a fake server read it.
pub struct Request {
    /// The HTTP method, such as `POST`.
    pub method: String,
    /// Path and query, as in the request line.
    pub path: String,
    headers: Vec<(String, String)>,
    /// The body, as many bytes as `Content-Length` says.
    pub body: Vec<u8>,
}

impl Request {
    /// The value of the header `name`, matched without regard to case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The request line and the headers, one per line, as they were sent.
    pub fn head_text(&self) -> String {
        let mut text = format!("{} {}\n", self.method, self.path);
        for (name, value) in &self.headers {
            text.push_str(&format!("{name}: {value}\n"));
        }
        text
    }

    /// The body as JSON; `Null` when empty.
    pub fn json(&self) -> serde_json::Value {
        if self.body.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&self.body).unwrap()
        }
    }
}

/// Read one request; `None` if the client closed the connection first.
pub async fn read_request(stream: &mut TcpStream) -> Option<Request> {
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
    let mut lines = head.lines();
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_string();
    let path = request_line.next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            Some((key.trim().to_string(), value.trim().to_string()))
        })
        .collect();
    let length: usize = headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
        .map_or(0, |(_, value)| value.parse().unwrap());
    while data.len() < header_end + length {
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        data.extend_from_slice(&buf[..n]);
    }
    Some(Request {
        method,
        path,
        headers,
        body: data[header_end..header_end + length].to_vec(),
    })
}

/// A complete response that closes the connection. `status` is the code and reason, such as
/// `"200 OK"`; `headers` are written as given.
pub fn response(status: &str, headers: &[(&str, String)], body: &str) -> String {
    let extra: String = headers
        .iter()
        .map(|(key, value)| format!("{key}: {value}\r\n"))
        .collect();
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json; charset=utf-8\r\ncontent-length: {}\r\n{extra}connection: close\r\n\r\n{body}",
        body.len()
    )
}

/// A complete response of `body`, bytes of `content_type`, that closes the connection.
pub fn response_bytes(status: &str, content_type: &str, body: &[u8]) -> Vec<u8> {
    let head = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

/// A listener that accepts connections and never answers them, for a request or a handshake
/// that does not end: its address, and a receiver told of each connection it accepts.
pub async fn silent_listener() -> (String, mpsc::UnboundedReceiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let (accepted, connections) = mpsc::unbounded_channel();
    // Ends with the test's runtime; until then the connections stay open, unanswered.
    tokio::spawn(async move {
        let mut open = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            open.push(stream);
            // The test may no longer be waiting for connections.
            accepted.send(()).ok();
        }
    });
    (address, connections)
}
