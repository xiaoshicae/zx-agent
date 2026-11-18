//! The MCP servers of config.toml, connected once for every session of a process.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use zx_agent::builtin::mcp::{Server, ServerConfig};
use zx_agent::extensions::Extension;
use zx_util::env::env_value;

use super::settings::Settings;

/// The MCP servers of config.toml, connected once per process and shared by every session it
/// opens; none without `[[mcp]]`.
#[derive(Default)]
pub(super) struct McpTools {
    pub(super) servers: Vec<Arc<dyn Extension>>,
    /// The names the model calls the servers' tools by; which of them ask depends on the
    /// agent, so each agent decides when it is built.
    pub(super) names: Vec<String>,
    /// Variables holding the servers' tokens: hidden from commands, values redacted.
    pub(super) token_env: Vec<String>,
}

impl McpTools {
    /// Connect the MCP servers of `settings`, each within its own bound. With them, a notice
    /// for each server, for the host to show: its tools, its protocol and its origin, without
    /// the token or the URL's path.
    ///
    /// Fails when a server's token variable is unset, or a server cannot be reached in time or
    /// lacks a tool.
    pub(super) async fn connect(settings: &Settings) -> Result<(Self, Vec<String>)> {
        let mcp = &settings.config.mcp;
        let mut configs = Vec::new();
        for server in mcp {
            let token =
                match &server.token_env {
                    Some(name) => Some(env_value(name).with_context(|| {
                        format!("MCP server {}: {name} must hold its token (tokenEnv)", server.name)
                    })?),
                    None => None,
                };
            configs.push(ServerConfig {
                token,
                call_timeout: server.timeout_secs.map_or(settings.tool_timeout, Duration::from_secs),
                ..ServerConfig::new(server.name.as_str(), server.url.as_str(), server.tools.clone())
            });
        }
        // Each connection has its own bound, so the slowest server sets the startup time.
        let connected = futures::future::join_all(configs.into_iter().map(Server::connect)).await;
        let mut tools = Self::default();
        let mut notices = Vec::new();
        for (entry, server) in mcp.iter().zip(connected) {
            let server = server.map_err(anyhow::Error::msg)?;
            let names: Vec<&str> = server.tool_names().iter().map(|(_, name)| name.as_str()).collect();
            // The origin only: a path or query may hold a credential. `Server::connect` has parsed
            // the URL already, so the empty fallback is never shown.
            let origin = reqwest::Url::parse(&entry.url)
                .map(|url| url.origin().ascii_serialization())
                .unwrap_or_default();
            let plain = if entry.token_env.is_some() && origin.starts_with("http://") {
                "; the token is sent unencrypted"
            } else {
                ""
            };
            notices.push(format!(
                "[mcp] {}: {}; protocol {}; {origin}{plain}",
                entry.name,
                names.join(", "),
                server.protocol_version()
            ));
            tools.names.extend(names.into_iter().map(str::to_string));
            tools.token_env.extend(entry.token_env.iter().cloned());
            tools.servers.push(Arc::new(server));
        }
        Ok((tools, notices))
    }
}
