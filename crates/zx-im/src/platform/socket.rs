//! Opening the WebSocket a platform delivers its events over: directly, or through the tunnel
//! that an HTTP proxy opens with `CONNECT` (RFC 9110, section 9.3.6) when the given proxies
//! route its address through one. TLS, for `wss://`, runs inside the tunnel, end to end with
//! the platform. The environment's proxies are for HTTP clients: without proxies of its own,
//! a WebSocket goes directly, as it always has.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::client::{Request, Response};
use tokio_tungstenite::tungstenite::{self, Error};
use zx_agent::ai::{Proxies, Proxy};

use super::Socket;

/// Most bytes of a proxy's answer to `CONNECT`: a status line and a few headers.
const MAX_ANSWER_BYTES: usize = 8 * 1024;

/// Open the WebSocket at `url`, through `proxy`'s tunnel when it routes the address through
/// one. Errors are tungstenite's; a tunnel that is not opened is an I/O error saying why.
/// Bounded by the caller.
pub(super) async fn open_socket(url: &str, proxy: &Proxy) -> Result<(Socket, Response), Error> {
    let request = url.into_client_request()?;
    let tunnel = match proxy {
        Proxy::Through(proxies) => tunnel_of(&request, proxies),
        Proxy::FromEnvironment | Proxy::Direct => None,
    };
    let Some((proxy, destination)) = tunnel else {
        return tokio_tungstenite::connect_async(request).await;
    };
    let stream = open_tunnel(proxy, &destination).await?;
    tokio_tungstenite::client_async_tls_with_config(request, stream, None, None).await
}

/// The proxy `proxies` routes the address of `request` through, and the address as
/// `CONNECT` names it, `host:port`; `None` when it goes directly, as an address that is
/// not a WebSocket's does, for tungstenite to refuse.
fn tunnel_of<'a>(request: &Request, proxies: &'a Proxies) -> Option<(&'a str, String)> {
    let uri = request.uri();
    let scheme = uri.scheme_str()?;
    let host = uri.host()?;
    let port = match (uri.port_u16(), scheme) {
        (Some(port), _) => port,
        (None, "wss") => 443,
        (None, "ws") => 80,
        (None, _) => return None,
    };
    let proxy = proxies.route(scheme, host)?;
    Some((proxy, format!("{host}:{port}")))
}

/// A connection to the proxy at `proxy`, an `http://` URL, that it tunnels to `destination`;
/// its credentials go with the request when the URL carries them.
async fn open_tunnel(proxy: &str, destination: &str) -> Result<TcpStream, Error> {
    // Checked when the configuration was; the error does not quote the URL, which may hold
    // a password.
    let proxy = reqwest::Url::parse(proxy).map_err(|error| no_tunnel(format!("invalid proxy: {error}")))?;
    let host = proxy.host_str().ok_or_else(|| no_tunnel("the proxy has no host"))?;
    let port = proxy
        .port_or_known_default()
        .ok_or_else(|| no_tunnel("the proxy has no port"))?;
    // A socket address takes an IPv6 address without the brackets of a URL.
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let mut stream = TcpStream::connect((host, port)).await?;
    let authorization = match basic_credentials(&proxy) {
        Some(credentials) => format!("Proxy-Authorization: Basic {credentials}\r\n"),
        None => String::new(),
    };
    let ask = format!("CONNECT {destination} HTTP/1.1\r\nHost: {destination}\r\n{authorization}\r\n");
    stream.write_all(ask.as_bytes()).await?;
    let status = answer_status(&mut stream).await?;
    if !(200..300).contains(&status) {
        return Err(no_tunnel(format!("the proxy refused the tunnel: HTTP {status}")));
    }
    Ok(stream)
}

/// The status of the proxy's answer to `CONNECT`, read to the empty line that ends it, and
/// no further: until the client speaks, a proxy that opened the tunnel sends nothing more.
async fn answer_status(stream: &mut TcpStream) -> Result<u16, Error> {
    let mut answer = Vec::new();
    let mut chunk = [0; 1024];
    let end = loop {
        if let Some(end) = answer.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
        if answer.len() > MAX_ANSWER_BYTES {
            return Err(no_tunnel(format!(
                "the proxy's answer to CONNECT is longer than {MAX_ANSWER_BYTES} bytes"
            )));
        }
        let read = stream.read(&mut chunk).await?;
        let Some(read) = chunk.get(..read).filter(|read| !read.is_empty()) else {
            return Err(no_tunnel("the proxy closed the connection before answering CONNECT"));
        };
        answer.extend_from_slice(read);
    };
    if end + 4 != answer.len() {
        return Err(no_tunnel("the proxy sent more than its answer to CONNECT"));
    }
    // `HTTP/1.1 200 Connection established`; an answer that is not text, or whose status is
    // not a number, is not HTTP.
    let status = std::str::from_utf8(&answer)
        .ok()
        .and_then(|text| text.strip_prefix("HTTP/1."))
        .and_then(|text| text.split(' ').nth(1))
        .and_then(|code| code.parse().ok());
    status.ok_or_else(|| no_tunnel("the proxy's answer to CONNECT is not HTTP"))
}

/// The user and password `proxy` carries, as `Proxy-Authorization: Basic` sends them:
/// `user:password`, their escapes decoded, in base64 (RFC 7617); `None` without either.
fn basic_credentials(proxy: &reqwest::Url) -> Option<String> {
    if proxy.username().is_empty() && proxy.password().is_none() {
        return None;
    }
    let mut pair = unescaped(proxy.username());
    pair.push(b':');
    pair.extend(proxy.password().map_or_else(Vec::new, unescaped));
    Some(STANDARD.encode(pair))
}

/// `text`, a user or password as a URL writes it, with each `%XX` escape turned back into
/// the byte it stands for.
fn unescaped(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        let escaped = text
            .get(index + 1..index + 3)
            .filter(|hex| byte == b'%' && hex.bytes().all(|digit| digit.is_ascii_hexdigit()))
            // Two hex digits always parse.
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(value) => {
                decoded.push(value);
                index += 3;
            }
            None => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    decoded
}

/// A tunnel that was not opened, as tungstenite reports a connection that failed.
fn no_tunnel(reason: impl Into<String>) -> Error {
    tungstenite::Error::Io(std::io::Error::other(reason.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxies(no_proxy: &[&str]) -> Proxies {
        Proxies {
            http: Some("http://plain.proxy:3128".into()),
            https: Some("http://tunnel.proxy:3128".into()),
            no_proxy: no_proxy.iter().map(ToString::to_string).collect(),
        }
    }

    fn tunnel(url: &str, proxies: &Proxies) -> Option<(String, String)> {
        let request = url.into_client_request().unwrap();
        tunnel_of(&request, proxies).map(|(proxy, destination)| (proxy.to_string(), destination))
    }

    #[test]
    fn a_websocket_goes_through_the_proxy_of_its_scheme_to_its_host_and_port() {
        let proxies = proxies(&["internal.example"]);
        assert_eq!(
            tunnel("wss://wss-primary.slack.com/link?ticket=1", &proxies),
            Some(("http://tunnel.proxy:3128".into(), "wss-primary.slack.com:443".into()))
        );
        assert_eq!(
            tunnel("ws://127.0.0.1:8080/socket", &proxies),
            Some(("http://plain.proxy:3128".into(), "127.0.0.1:8080".into()))
        );
        assert_eq!(
            tunnel("wss://[::1]:9443/", &proxies),
            Some(("http://tunnel.proxy:3128".into(), "[::1]:9443".into()))
        );
        assert_eq!(tunnel("wss://events.internal.example/", &proxies), None);
    }

    #[test]
    fn the_credentials_are_the_decoded_user_and_password_in_base64() {
        let url = |text: &str| reqwest::Url::parse(text).unwrap();
        // RFC 7617's example, with the space escaped as a URL writes it.
        assert_eq!(
            basic_credentials(&url("http://Aladdin:open%20sesame@proxy:3128")).as_deref(),
            Some("QWxhZGRpbjpvcGVuIHNlc2FtZQ==")
        );
        assert_eq!(
            basic_credentials(&url("http://token@proxy:3128")).as_deref(),
            Some("dG9rZW46")
        );
        // `a@b:c`: the user's escapes are decoded too.
        assert_eq!(
            basic_credentials(&url("http://a%40b:c@proxy:3128")).as_deref(),
            Some("YUBiOmM=")
        );
        assert_eq!(basic_credentials(&url("http://proxy:3128")), None);
        // A `%` without two hex digits after it stands for itself.
        assert_eq!(unescaped("100%+1%2"), b"100%+1%2");
        assert_eq!(unescaped("p%40ss%2F"), b"p@ss/");
    }
}
