//! An HTTP proxy on this machine: it forwards a request written with an absolute URL to the
//! server the URL names, and opens a tunnel for `CONNECT`, piping both ways; it records what
//! each client asked for, with the credentials it sent.

use std::sync::{Arc, Mutex};

use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

use crate::http::{Request, read_request};

/// What a client asked the proxy for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asked {
    /// `CONNECT host:port`, or the method and the absolute URL of a request it forwarded.
    pub line: String,
    /// The `Proxy-Authorization` the client sent.
    pub authorization: Option<String>,
}

/// A running proxy, until the test's runtime ends.
pub struct FakeProxy {
    /// Where it listens, `127.0.0.1:<port>`.
    pub address: String,
    asked: Arc<Mutex<Vec<Asked>>>,
}

impl FakeProxy {
    /// Start listening.
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let asked = Arc::new(Mutex::new(Vec::new()));
        let log = asked.clone();
        // Ends with the test's runtime, as do the connections it serves.
        tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                tokio::spawn(serve(client, log.clone()));
            }
        });
        Self { address, asked }
    }

    /// Its URL, `http://` with `user_info` (`user:password`, escaped as a URL writes it)
    /// before the address when given.
    pub fn url(&self, user_info: Option<&str>) -> String {
        match user_info {
            Some(user_info) => format!("http://{user_info}@{}", self.address),
            None => format!("http://{}", self.address),
        }
    }

    /// What the clients have asked for so far, in order.
    pub fn asked(&self) -> Vec<Asked> {
        self.asked.lock().unwrap().clone()
    }
}

/// Serve one client: record its request, then tunnel or forward it.
async fn serve(mut client: TcpStream, asked: Arc<Mutex<Vec<Asked>>>) {
    let Some(request) = read_request(&mut client).await else {
        return;
    };
    asked.lock().unwrap().push(Asked {
        line: format!("{} {}", request.method, request.path),
        authorization: request.header("proxy-authorization").map(str::to_string),
    });
    let (destination, head) = if request.method == "CONNECT" {
        (request.path.clone(), None)
    } else {
        let (destination, head) = origin_form(&request);
        (destination, Some(head))
    };
    let Ok(mut server) = TcpStream::connect(destination.as_str()).await else {
        // The client may be gone.
        client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await.ok();
        return;
    };
    match head {
        None => client
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .await
            .unwrap(),
        Some(head) => {
            server.write_all(&head).await.unwrap();
            server.write_all(&request.body).await.unwrap();
        }
    }
    // Ends when either side closes.
    tokio::io::copy_bidirectional(&mut client, &mut server).await.ok();
}

/// The server a request with an absolute URL is for, `host:port`, and the request's head as
/// that server takes it: the path alone, without the headers meant for the proxy.
fn origin_form(request: &Request) -> (String, Vec<u8>) {
    let rest = request.path.strip_prefix("http://").unwrap();
    let (destination, path) = match rest.split_once('/') {
        Some((destination, path)) => (destination, format!("/{path}")),
        None => (rest, "/".to_string()),
    };
    let mut head = format!("{} {path} HTTP/1.1\r\n", request.method);
    for (name, value) in request.headers() {
        if !name.to_ascii_lowercase().starts_with("proxy-") {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    head.push_str("\r\n");
    (destination.to_string(), head.into_bytes())
}
