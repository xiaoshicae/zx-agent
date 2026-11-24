//! How a program's own connections reach the network: through the proxies the environment
//! names, directly, or through proxies its configuration gives. The commands an agent runs are
//! not among them: they see the environment as it is.

use std::fmt;
use std::net::IpAddr;

/// How the connections of a provider, an MCP client or a chat platform reach the network.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Proxy {
    /// Through the proxies of the environment, as the HTTP client reads them: `HTTP_PROXY`,
    /// `HTTPS_PROXY`, `ALL_PROXY` and `NO_PROXY`. A WebSocket goes direct.
    #[default]
    FromEnvironment,
    /// Directly, whatever the environment says.
    Direct,
    /// Through the proxies given, which are all there is.
    Through(Proxies),
}

/// Proxies a configuration gives. Shown without their URLs, which may hold a password.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Proxies {
    /// The proxy of `http://` and `ws://` destinations, as `http://[user:password@]host:port`.
    pub http: Option<String>,
    /// The proxy of `https://` and `wss://` destinations, which it tunnels to, written the
    /// same way.
    pub https: Option<String>,
    /// The hosts reached directly, each entry one or several separated by commas, as
    /// `NO_PROXY` writes them: a name covers itself and its subdomains, with or without a
    /// leading `.`; an IP address covers itself, a CIDR block (`10.0.0.0/8`) the addresses in
    /// it; `*` covers every host. Addresses are compared as the destination writes them: a
    /// name is not resolved.
    pub no_proxy: Vec<String>,
}

impl Proxies {
    /// The proxy of a destination of `scheme` on `host`, as a URL: `None` when it goes
    /// directly, for want of a proxy of its scheme or because `no_proxy` covers the host.
    pub fn route(&self, scheme: &str, host: &str) -> Option<&str> {
        let proxy = match scheme {
            "http" | "ws" => self.http.as_deref(),
            "https" | "wss" => self.https.as_deref(),
            _ => None,
        }?;
        (!self.bypasses(host)).then_some(proxy)
    }

    /// Whether `no_proxy` covers `host`, compared without case and, for an IPv6 address,
    /// without its brackets.
    fn bypasses(&self, host: &str) -> bool {
        let host = bare(host);
        let address = host.parse::<IpAddr>();
        self.no_proxy
            .iter()
            .flat_map(|entry| entry.split(','))
            .map(|entry| bare(entry.trim().trim_start_matches('.')))
            .any(|entry| match (&address, block(&entry)) {
                (Ok(address), Some((network, prefix))) => within(*address, network, prefix),
                _ => {
                    !entry.is_empty()
                        && (entry == "*"
                            || host == entry
                            || host
                                .strip_suffix(entry.as_str())
                                .is_some_and(|start| start.ends_with('.')))
                }
            })
    }
}

/// The network and prefix length of `entry` when it is a CIDR block, such as `10.0.0.0/8`.
fn block(entry: &str) -> Option<(IpAddr, u32)> {
    let (network, prefix) = entry.split_once('/')?;
    // An entry that does not parse as a block is compared as a name.
    let network: IpAddr = network.parse().ok()?;
    let prefix: u32 = prefix.parse().ok()?;
    let bits = if network.is_ipv4() { 32 } else { 128 };
    (prefix <= bits).then_some((network, prefix))
}

/// Whether `address` is in the block of `network` with a prefix of `prefix` bits, which is
/// at most the width of the address.
fn within(address: IpAddr, network: IpAddr, prefix: u32) -> bool {
    match (address, network) {
        (IpAddr::V4(address), IpAddr::V4(network)) => {
            let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
            u32::from(address) & mask == u32::from(network) & mask
        }
        (IpAddr::V6(address), IpAddr::V6(network)) => {
            let mask = if prefix == 0 { 0 } else { u128::MAX << (128 - prefix) };
            u128::from(address) & mask == u128::from(network) & mask
        }
        // An IPv4 address is never in an IPv6 block, nor the other way round.
        (IpAddr::V4(_), IpAddr::V6(_)) | (IpAddr::V6(_), IpAddr::V4(_)) => false,
    }
}

impl fmt::Debug for Proxies {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let given = |url: &Option<String>| if url.is_some() { "given" } else { "none" };
        formatter
            .debug_struct("Proxies")
            .field("http", &given(&self.http))
            .field("https", &given(&self.https))
            .field("no_proxy", &self.no_proxy)
            .finish()
    }
}

/// `host` in lowercase, without the brackets of an IPv6 address.
fn bare(host: &str) -> String {
    host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxies(no_proxy: &[&str]) -> Proxies {
        Proxies {
            http: Some("http://plain.proxy:3128".into()),
            https: Some("http://user:pass@tunnel.proxy:3128".into()),
            no_proxy: no_proxy.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn each_scheme_goes_through_its_proxy() {
        let proxies = proxies(&[]);
        assert_eq!(proxies.route("http", "example.com"), Some("http://plain.proxy:3128"));
        assert_eq!(proxies.route("ws", "example.com"), Some("http://plain.proxy:3128"));
        assert_eq!(
            proxies.route("https", "example.com"),
            Some("http://user:pass@tunnel.proxy:3128")
        );
        assert_eq!(
            proxies.route("wss", "example.com"),
            Some("http://user:pass@tunnel.proxy:3128")
        );
        assert_eq!(proxies.route("ftp", "example.com"), None);
        let https_only = Proxies { http: None, ..proxies };
        assert_eq!(https_only.route("http", "example.com"), None);
    }

    #[test]
    fn the_hosts_of_no_proxy_go_directly_with_their_subdomains() {
        let proxies = proxies(&[
            "internal.example",
            ".svc.cluster.local",
            "10.0.0.1",
            "[::1]",
            "LocalHost",
        ]);
        for host in [
            "internal.example",
            "api.internal.example",
            "db.prod.svc.cluster.local",
            "10.0.0.1",
            "[::1]",
            "localhost",
        ] {
            assert_eq!(proxies.route("https", host), None, "{host}");
        }
        for host in ["notinternal.example", "example", "10.0.0.10", "cluster.local.evil"] {
            assert!(proxies.route("https", host).is_some(), "{host}");
        }
        assert_eq!(self::proxies(&["*"]).route("http", "anything"), None);
        // An empty entry covers nothing.
        assert!(self::proxies(&["", "."]).route("http", "example.").is_some());
    }

    #[test]
    fn an_entry_may_list_hosts_as_no_proxy_does_and_a_block_covers_its_addresses() {
        let proxies = proxies(&["localhost, .svc.cluster.local,10.0.0.0/8", "fd00::/8", "192.168.1.7/32"]);
        for host in [
            "localhost",
            "api.svc.cluster.local",
            "10.1.2.3",
            "[fd12::1]",
            "192.168.1.7",
        ] {
            assert_eq!(proxies.route("https", host), None, "{host}");
        }
        for host in ["11.0.0.1", "[fe80::1]", "192.168.1.8", "10.example.com"] {
            assert!(proxies.route("https", host).is_some(), "{host}");
        }
        assert_eq!(self::proxies(&["0.0.0.0/0"]).route("https", "8.8.8.8"), None);
        // Not a block: compared as a name, which no address is.
        assert!(self::proxies(&["10.0.0.0/33"]).route("https", "10.0.0.1").is_some());
    }

    #[test]
    fn debug_output_shows_whether_a_proxy_is_given_but_not_its_url() {
        let shown = format!("{:?}", proxies(&[]));
        assert!(!shown.contains("pass") && shown.contains("given"), "{shown}");
    }
}
