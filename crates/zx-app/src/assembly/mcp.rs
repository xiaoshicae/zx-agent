//! The MCP servers of config.toml, connected once for every session of a process.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use zx_agent::builtin::mcp::{Server, ServerConfig};
use zx_agent::extensions::Extension;
use zx_agent::tools::Effects;
use zx_util::env::env_value;

use super::settings::Settings;
use crate::config::McpServer;

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
    /// Variables holding the servers' tokens and the values of their `headerEnv`: hidden from
    /// commands, values redacted.
    pub(super) secret_env: Vec<String>,
}

impl McpTools {
    /// Connect the MCP servers of `settings`, each within its own bound. With them, a notice
    /// for each server, for the host to show: its tools, those it is trusted to say only read
    /// marked, its protocol and its origin, without the token, the headers or the URL's path.
    ///
    /// Fails when a variable of a server's `tokenEnv` or `headerEnv` is unset, or a server
    /// cannot be reached in time or lacks a tool.
    pub(super) async fn connect(settings: &Settings) -> Result<(Self, Vec<String>)> {
        let mcp = &settings.config.mcp;
        let configs = mcp
            .iter()
            .map(|server| server_config(server, settings.tool_timeout))
            .collect::<Result<Vec<_>>>()?;
        // Each connection has its own bound, so the slowest server sets the startup time.
        let connected = futures::future::join_all(configs.into_iter().map(Server::connect)).await;
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
            let credentials = entry.token_env.is_some() || !entry.header_env.is_empty();
            let plain = if credentials && origin.starts_with("http://") {
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
            tools.secret_env.extend(entry.token_env.iter().cloned());
            tools.secret_env.extend(entry.header_env.values().cloned());
            tools.servers.push(Arc::new(server));
        }
        Ok((tools, notices))
    }
}

/// The settings to connect `server` with: its token and the values of its `headerEnv` read
/// from their variables, and calls bounded by its `timeoutSecs` or else by `tool_timeout`.
fn server_config(server: &McpServer, tool_timeout: Duration) -> Result<ServerConfig> {
    let name = server.name.as_str();
    let token = match &server.token_env {
        Some(variable) => Some(
            env_value(variable)
                .with_context(|| format!("MCP server {name}: {variable} must hold its token (tokenEnv)"))?,
        ),
        None => None,
    };
    let headers = server.sent_headers(|header, variable| {
        env_value(variable).with_context(|| {
            format!("MCP server {name}: {variable} must hold the value of header {header} (headerEnv)")
        })
    })?;
    Ok(ServerConfig {
        token,
        headers,
        trust_read_only: server.trust_read_only,
        call_timeout: server.timeout_secs.map_or(tool_timeout, Duration::from_secs),
        ..ServerConfig::new(name, server.url.as_str(), server.tools.clone())
    })
}
