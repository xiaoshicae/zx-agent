//! One HTTP/1.1 request per connection, read and answered, for the fake platform servers.

use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

pub struct Request {
    pub method: String,
    /// Path and query, as in the request line.
    pub path: String,
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
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
