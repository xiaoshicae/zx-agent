//! The MCP servers of config.toml, connected once for every session of a process.

use std::sync::Arc;

use anyhow::Result;
use zx_agent::builtin::mcp::Server;
use zx_agent::tools::{Effects, Tool};

use super::settings::Settings;
use crate::config::{Credentials, McpServer};

/// The MCP servers of config.toml, connected once per process and shared by every session it
/// opens; none without `[[mcp]]`.
pub(super) struct McpTools {
    /// The servers' tools, offered to every agent, in the order of `[[mcp]]`. Which of them
    /// ask depends on the agent, so each agent decides when it is built: by their names, and
    /// by which of them a server trusted with `trustReadOnly` says only read
    /// ([`Effects::ReadOnly`]).
    pub(super) tools: Vec<Arc<dyn Tool>>,
}

impl McpTools {
    /// Connect the MCP servers of `settings`, each within its own bound. With them, a notice
    /// for each server, for the host to show: its tools, those it is trusted to say only read
    /// marked, its protocol and its origin, without the token, the headers or the URL's path.
    ///
    /// Fails when a server cannot be reached in time or lacks a tool.
    pub(super) async fn connect(settings: &Settings) -> Result<(Self, Vec<String>)> {
        let config = &settings.config;
        // Each connection has its own bound, so the slowest server sets the startup time.
        let connected = futures::future::join_all(config.mcp_servers().map(Server::connect)).await;
        let mut tools = Vec::new();
        let mut notices = Vec::new();
        for (entry, server) in config.mcp.iter().zip(connected) {
            let server = server.map_err(anyhow::Error::msg)?;
            let offered = server.tools();
            let listed: Vec<String> = offered
                .iter()
                .map(|tool| match tool.effects() {
                    Effects::ReadOnly => format!("{} (read-only)", tool.name()),
                    Effects::SideEffects => tool.name().to_string(),
                })
                .collect();
            let origin = server.origin();
            let plain = if sends_credentials(entry, &config.credentials) && origin.starts_with("http://") {
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
            tools.extend(offered);
        }
        Ok((Self { tools }, notices))
    }
}

/// Whether `server` sends a credential: its token, or a header whose value is among
/// `credentials`, as that of a header that names a variable in config.toml is.
fn sends_credentials(server: &McpServer, credentials: &Credentials) -> bool {
    server.token.is_some() || server.headers.values().any(|value| credentials.values.contains(value))
}
