//! The MCP servers of config.toml, connected once for every session of a process.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use zx_agent::ai::Proxy;
use zx_agent::builtin::mcp::{Server, ServerConfig};
use zx_agent::extensions::Extension;
use zx_agent::tools::Effects;

use super::settings::Settings;
use crate::config::{Credentials, McpServer};

/// The MCP servers of config.toml, connected once per process and shared by every session it
/// opens; none without `[[mcp]]`.
#[derive(Default)]
pub(super) struct McpTools {
    pub(super) servers: Vec<Arc<dyn Extension>>,
    /// The names the model calls the servers' tools by; which of them ask depends on the
    /// agent, so each agent decides when it is built.
    pub(super) names: Vec<String>,
    /// Those of `names` that a server trusted with `trustReadOnly` says only read: they ask
    /// only where the agent's `permissions.ask` lists them.
    pub(super) read_only: Vec<String>,
}

impl McpTools {
    /// Connect the MCP servers of `settings`, each within its own bound. With them, a notice
    /// for each server, for the host to show: its tools, those it is trusted to say only read
    /// marked, its protocol and its origin, without the token, the headers or the URL's path.
    ///
    /// Fails when a server cannot be reached in time or lacks a tool.
    pub(super) async fn connect(settings: &Settings) -> Result<(Self, Vec<String>)> {
        let mcp = &settings.config.mcp;
        let configs = mcp
            .iter()
            .map(|server| server_config(server, settings.tool_timeout, &settings.config.proxy));
        // Each connection has its own bound, so the slowest server sets the startup time.
        let connected = futures::future::join_all(configs.map(Server::connect)).await;
        let mut tools = Self::default();
        let mut notices = Vec::new();
        for (entry, server) in mcp.iter().zip(connected) {
            let server = server.map_err(anyhow::Error::msg)?;
            let read_only: Vec<String> = server
                .tools()
                .iter()
                .filter(|tool| tool.effects() == Effects::ReadOnly)
                .map(|tool| tool.name().to_string())
                .collect();
            let listed: Vec<String> = server
                .tool_names()
                .iter()
                .map(|name| {
                    if read_only.contains(name) {
                        format!("{name} (read-only)")
                    } else {
                        name.clone()
                    }
                })
                .collect();
            let origin = server.origin();
            let plain = if sends_credentials(entry, &settings.config.credentials) && origin.starts_with("http://") {
                "; credentials are sent unencrypted"
            } else {
                ""
            };
            notices.push(format!(
                "[mcp] {}: {}; protocol {}; {origin}{plain}",
                entry.name,
                listed.join(", "),
                server.protocol_version()
            ));
            tools.names.extend(server.tool_names().iter().cloned());
            tools.read_only.extend(read_only);
            tools.servers.push(Arc::new(server));
        }
        Ok((tools, notices))
    }
}

/// Whether `server` sends a credential among `credentials`: its token, or a header that names
/// a variable.
fn sends_credentials(server: &McpServer, credentials: &Credentials) -> bool {
    server.token.is_some() || server.headers.values().any(|value| credentials.values.contains(value))
}

/// The settings to connect `server` with: its token and headers, calls bounded by its
/// `timeoutSecs` or else by `tool_timeout`, and requests sent through `proxy`.
fn server_config(server: &McpServer, tool_timeout: Duration, proxy: &Proxy) -> ServerConfig {
    ServerConfig {
        token: server.token.clone(),
        headers: server.headers.clone().into_iter().collect(),
        trust_read_only: server.trust_read_only,
        call_timeout: server.timeout_secs.map_or(tool_timeout, Duration::from_secs),
        proxy: proxy.clone(),
        ..ServerConfig::new(server.name.as_str(), server.url.as_str(), server.tools.clone())
    }
}
