//! The proxies an HTTP client sends through ([`Proxy`]), for the providers and for the other
//! clients of a program: its MCP servers, its chat platforms and its trace export.

use zx_ai::{Proxies, Proxy};

/// `builder` sending through `proxy`. The environment's proxies are the client's default;
/// given proxies replace them (a client with a proxy of its own reads no environment), and
/// [`Proxies::route`] decides each request, as it decides a tunnel of the program's own.
/// Fails for a given proxy that [`check_proxy`] refuses.
pub fn with_proxy(builder: reqwest::ClientBuilder, proxy: &Proxy) -> Result<reqwest::ClientBuilder, String> {
    Ok(match proxy {
        Proxy::FromEnvironment => builder,
        Proxy::Direct => builder.no_proxy(),
        Proxy::Through(proxies) => builder.proxy(routed(proxies)?),
    })
}

/// The proxy of a client that sends through `proxies`, deciding each request by
/// [`Proxies::route`], for a client that [`with_proxy`] does not build, such as a blocking
/// one. Fails for a proxy that [`check_proxy`] refuses, which the client would otherwise
/// pass over, going directly.
pub fn routed(proxies: &Proxies) -> Result<reqwest::Proxy, String> {
    for (scheme, url) in [("http", &proxies.http), ("https", &proxies.https)] {
        if let Some(url) = url {
            check_proxy(url).map_err(|error| format!("the proxy of {scheme}:// destinations {error}"))?;
        }
    }
    let proxies = proxies.clone();
    Ok(reqwest::Proxy::custom(move |url| {
        // Every URL a client sends to has a host; an empty one is covered by no entry of
        // `no_proxy` but `*`.
        let host = url.host_str().unwrap_or_default();
        proxies.route(url.scheme(), host).map(str::to_string)
    }))
}

/// Check `url`, the address of a proxy: `http://[user:password@]host[:port]`, with nothing
/// after it but `/`. The program speaks plain HTTP to its proxy, which tunnels `https://`
/// and `wss://` destinations. The error, which goes after the proxy's name, does not quote
/// the URL: it may hold a password.
pub fn check_proxy(url: &str) -> Result<(), String> {
    // Without a scheme, `host:port` would parse as one named after the host.
    if !url.contains("://") {
        return Err("must be the http:// URL of the proxy, as http://host:port".to_string());
    }
    let parsed = reqwest::Url::parse(url).map_err(|error| format!("is not a URL: {error}"))?;
    if parsed.scheme() != "http" {
        return Err(format!(
            "must be an http:// URL of the proxy, not {}://",
            parsed.scheme()
        ));
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err("has no host".to_string());
    }
    if !matches!(parsed.path(), "" | "/") || parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("must end with the host and port".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_proxy_is_an_http_url_of_a_host_and_port() {
        for url in [
            "http://proxy.internal:3128",
            "http://proxy.internal:3128/",
            "http://user:p%40ss@10.0.0.1:8080",
            "http://[::1]:3128",
            "http://proxy.internal",
        ] {
            assert_eq!(check_proxy(url), Ok(()), "{url}");
        }
        for (url, error) in [
            (
                "proxy.internal:3128",
                "must be the http:// URL of the proxy, as http://host:port",
            ),
            ("http://:3128", "is not a URL"),
            (
                "https://proxy.internal:3128",
                "must be an http:// URL of the proxy, not https://",
            ),
            ("socks5://proxy.internal:1080", "not socks5://"),
            ("http://proxy.internal:3128/path", "must end with the host and port"),
            ("http://proxy.internal:3128?a=b", "must end with the host and port"),
        ] {
            let found = check_proxy(url).unwrap_err();
            assert!(found.starts_with(error) || found.contains(error), "{url}: {found}");
        }
    }

    #[test]
    fn an_error_does_not_quote_the_password() {
        let error = with_proxy(
            reqwest::Client::builder(),
            &Proxy::Through(Proxies {
                https: Some("https://user:secret@proxy.internal".into()),
                ..Proxies::default()
            }),
        )
        .err()
        .unwrap();
        assert_eq!(
            error,
            "the proxy of https:// destinations must be an http:// URL of the proxy, not https://"
        );
    }
}
