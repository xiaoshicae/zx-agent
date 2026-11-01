//! A client for external MCP servers that only lists and calls their tools, over Streamable
//! HTTP (docs/p7-mcp-review.md). The servers, and which of their tools the model gets, come
//! from configuration; whether a call may run is for the host's policies to decide, not for
//! the server: its tool annotations are only hints.
//!
//! A [`Server`] is an [`Extension`]: register it with [`crate::extensions::register`].

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig, ContentBlock, Implementation,
    ProtocolVersion, ResourceContents,
};
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ClientHandler, ClientLifecycleMode, RoleClient, serve_client_with_lifecycle};
use serde_json::Value;

use crate::agent::{Tool, ToolContext, ToolOutput};
use crate::ai::ToolDef;
use crate::extensions::Extension;
use crate::tools::truncate::{MAX_BYTES, MAX_LINES, truncate_head};

/// The longest tool name that OpenAI-compatible APIs accept: "a-z, A-Z, 0-9, or contain
/// underscores and dashes, with a maximum length of 64" (openai-openapi, `FunctionObject`).
pub const MAX_TOOL_NAME: usize = 64;
/// Tool descriptions go with every request and are written by the server, so they are cut
/// to this size.
pub const MAX_DESCRIPTION_BYTES: usize = 4 * 1024;
/// Default bound for connecting and listing the tools.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Default bound for one tool call.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// One MCP server and the tools to take from it. Build it with [`ServerConfig::new`] and
/// struct update syntax. It holds the credential, so it does not implement `Debug`.
pub struct ServerConfig {
    /// ASCII lowercase letters, digits and `-`. Tools reach the model as `<name>__<tool>`.
    pub name: String,
    /// The server's MCP endpoint, `http://` or `https://`.
    pub url: String,
    /// Sent as `Authorization: Bearer <token>` with every request; `None` sends none.
    pub token: Option<String>,
    /// The server's tools to offer the model, by the names the server gives them. The others
    /// are left out, descriptions included. Not empty.
    pub tools: Vec<String>,
    /// Bound for connecting and listing the tools.
    pub connect_timeout: Duration,
    /// Bound for each tool call.
    pub call_timeout: Duration,
}

impl ServerConfig {
    pub fn new(name: impl Into<String>, url: impl Into<String>, tools: Vec<String>) -> Self {
        Self {
            name: name.into(),
            url: url.into(),
            token: None,
            tools,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            call_timeout: DEFAULT_CALL_TIMEOUT,
        }
    }
}

type Client = RunningService<RoleClient, Handler>;

/// A connected MCP server and the tools taken from it.
pub struct Server {
    name: String,
    protocol_version: String,
    tools: Vec<Arc<dyn Tool>>,
    /// `(the server's name, the model's name)` of each tool, in configuration order.
    names: Vec<(String, String)>,
}

impl Server {
    /// Connect, settle the protocol version, list the server's tools and keep the configured
    /// ones. The server is asked for `server/discover` first (protocol 2026-07-28) and, if it
    /// answers as an older server does, for the `initialize` handshake (2025-11-25).
    ///
    /// Errors, each naming the server: a malformed name or URL, no configured tools, no answer
    /// within `connect_timeout`, a protocol or transport failure, a configured tool the server
    /// does not offer, and tool names that cannot be given to the model (see [`tool_name`]).
    /// The token never appears in them.
    pub async fn connect(config: ServerConfig) -> Result<Self, String> {
        let server = config.name.as_str();
        check_server_name(server)?;
        let url =
            reqwest::Url::parse(&config.url).map_err(|error| format!("MCP server {server}: invalid url: {error}"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(format!("MCP server {server}: the url must be http:// or https://"));
        }
        if config.tools.is_empty() {
            return Err(format!("MCP server {server}: no tools are configured"));
        }
        let mut transport = StreamableHttpClientTransportConfig::with_uri(config.url.as_str());
        if let Some(token) = &config.token {
            transport = transport.auth_header(token.as_str());
        }
        let transport = StreamableHttpClientTransport::from_config(transport);
        let lifecycle = ClientLifecycleMode::Auto {
            preferred_versions: vec![ProtocolVersion::LATEST],
            legacy_version: Some(ProtocolVersion::LATEST_WITH_INITIALIZE),
        };
        let connecting = async {
            let client = serve_client_with_lifecycle(Handler::new(), transport, lifecycle)
                .await
                .map_err(|error| error.to_string())?;
            let listed = client.list_all_tools().await.map_err(|error| error.to_string())?;
            Ok::<_, String>((client, listed))
        };
        let (client, listed) = match tokio::time::timeout(config.connect_timeout, connecting).await {
            Ok(Ok(connected)) => connected,
            Ok(Err(error)) => return Err(format!("MCP server {server}: {error}")),
            Err(_) => {
                return Err(format!(
                    "MCP server {server}: no answer within {} s",
                    config.connect_timeout.as_secs()
                ));
            }
        };
        let protocol_version = client
            .peer_info()
            .map(|info| info.protocol_version.to_string())
            .unwrap_or_default();
        let client = Arc::new(client);
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        let mut names = Vec::new();
        let mut taken = HashSet::new();
        for remote in &config.tools {
            let Some(offered) = listed.iter().find(|tool| tool.name.as_ref() == remote.as_str()) else {
                let offered: Vec<&str> = listed.iter().map(|tool| tool.name.as_ref()).collect();
                return Err(format!(
                    "MCP server {server}: it has no tool named {remote:?}; it offers: {}",
                    offered.join(", ")
                ));
            };
            let name = tool_name(server, remote)?;
            if !taken.insert(name.clone()) {
                return Err(format!(
                    "MCP server {server}: tool {remote:?} would be named {name:?}, like another of its tools"
                ));
            }
            tools.push(Arc::new(McpTool {
                definition: ToolDef {
                    name: name.clone(),
                    description: bounded(
                        offered.description.as_deref().unwrap_or_default(),
                        MAX_DESCRIPTION_BYTES,
                    ),
                    parameters: Value::Object(offered.input_schema.as_ref().clone()),
                },
                remote: remote.clone(),
                server: server.to_string(),
                client: Arc::clone(&client),
                timeout: config.call_timeout,
            }));
            names.push((remote.clone(), name));
        }
        Ok(Self {
            name: config.name,
            protocol_version,
            tools,
            names,
        })
    }

    /// The protocol version settled with the server, such as `2026-07-28`.
    pub fn protocol_version(&self) -> &str {
        &self.protocol_version
    }

    /// `(the server's name, the model's name)` of each tool, in configuration order.
    pub fn tool_names(&self) -> &[(String, String)] {
        &self.names
    }
}

impl Extension for Server {
    fn name(&self) -> &str {
        &self.name
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.clone()
    }
}

/// The name the model calls a server's tool by: `<server>__<tool>`, with characters other than
/// ASCII letters, digits, `_` and `-` replaced by `_`. MCP allows tool names of up to 128
/// characters, including `.`; model APIs allow fewer (see [`MAX_TOOL_NAME`]). A server name
/// has no `_`, so the first `__` ends it. Errors when the name would be too long.
pub fn tool_name(server: &str, tool: &str) -> Result<String, String> {
    let tool: String = tool
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let name = format!("{server}__{tool}");
    if name.len() > MAX_TOOL_NAME {
        return Err(format!(
            "MCP server {server}: tool name {name:?} is longer than {MAX_TOOL_NAME} characters"
        ));
    }
    Ok(name)
}

fn check_server_name(name: &str) -> Result<(), String> {
    let well_formed = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if well_formed {
        Ok(())
    } else {
        Err(format!(
            "MCP server name {name:?} must be ASCII lowercase letters, digits and -"
        ))
    }
}

/// `text`, cut at a character boundary within `max` bytes.
fn bounded(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} [description truncated]", &text[..end])
}

/// Identifies zx-agent and declares no optional capabilities: only tools are used.
struct Handler {
    info: ClientConfig,
}

impl Handler {
    fn new() -> Self {
        Self {
            info: ClientConfig::new(
                ClientCapabilities::default(),
                Implementation::new("zx-agent", env!("CARGO_PKG_VERSION")),
            ),
        }
    }
}

impl ClientHandler for Handler {
    fn get_info(&self) -> ClientConfig {
        self.info.clone()
    }
}

/// One of a server's tools, as the model sees it.
struct McpTool {
    definition: ToolDef,
    /// The server's name for the tool.
    remote: String,
    server: String,
    client: Arc<Client>,
    timeout: Duration,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.definition.name
    }

    fn definition(&self) -> ToolDef {
        self.definition.clone()
    }

    // `effects` stays `SideEffects`: the server's annotations are hints, so a call that was
    // interrupted is reported as possibly having changed state.

    fn validate_arguments(&self, args: &Value) -> Result<(), String> {
        if args.is_object() {
            Ok(())
        } else {
            Err("the arguments must be a JSON object".to_string())
        }
    }

    /// Stops waiting on `ctx.cancel` or after the timeout. Dropping the call makes rmcp end
    /// the request: it closes the stream and sends `notifications/cancelled`.
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, String> {
        let Value::Object(arguments) = args else {
            return Err("the arguments must be a JSON object".to_string());
        };
        let call = self
            .client
            .call_tool(CallToolRequestParams::new(self.remote.clone()).with_arguments(arguments));
        let answered = tokio::select! {
            answered = tokio::time::timeout(self.timeout, call) => answered,
            () = ctx.cancel.cancelled() => {
                return Err(format!("MCP server {}: the call to {} was cancelled", self.server, self.remote));
            }
        };
        let result = match answered {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => return Err(format!("MCP server {}: {} failed: {error}", self.server, self.remote)),
            Err(_) => {
                return Err(format!(
                    "MCP server {}: {} did not answer within {} s",
                    self.server,
                    self.remote,
                    self.timeout.as_secs()
                ));
            }
        };
        let text = render(&result);
        let kept = truncate_head(&text, MAX_LINES, MAX_BYTES);
        let mut content = kept.text;
        if kept.truncated {
            content.push_str(&format!(
                "\n[result truncated; showing {} of {} lines]",
                kept.kept_lines, kept.total_lines
            ));
        }
        Ok(ToolOutput {
            content,
            // Absent means false.
            is_error: result.is_error == Some(true),
            terminate: false,
        })
    }
}

/// The text the model gets: text blocks as they are, a line for content it cannot read, and
/// the structured result when there is no other content.
fn render(result: &CallToolResult) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in &result.content {
        parts.push(match block {
            ContentBlock::Text(text) => text.text.clone(),
            ContentBlock::Image(image) => format!("[image {} omitted]", image.mime_type),
            ContentBlock::Audio(audio) => format!("[audio {} omitted]", audio.mime_type),
            ContentBlock::ResourceLink(link) => format!("[resource {} ({})]", link.uri, link.name),
            ContentBlock::Resource(embedded) => match &embedded.resource {
                ResourceContents::TextResourceContents { uri, text, .. } => format!("[resource {uri}]\n{text}"),
                ResourceContents::BlobResourceContents { uri, mime_type, .. } => format!(
                    "[resource {uri}: binary {} omitted]",
                    mime_type.as_deref().unwrap_or("data")
                ),
                // `ResourceContents` is non-exhaustive.
                _ => "[resource of an unknown kind omitted]".to_string(),
            },
            // `ContentBlock` is non-exhaustive.
            _ => "[content of an unknown kind omitted]".to_string(),
        });
    }
    if parts.is_empty()
        && let Some(structured) = &result.structured_content
    {
        parts.push(structured.to_string());
    }
    if parts.is_empty() {
        return "(no content)".to_string();
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_are_prefixed_and_limited_to_what_model_apis_accept() {
        assert_eq!(tool_name("prom", "query").unwrap(), "prom__query");
        assert_eq!(tool_name("prom", "range.query v2").unwrap(), "prom__range_query_v2");
        assert_eq!(tool_name("prom", "中文").unwrap(), "prom____");
        let long = "x".repeat(MAX_TOOL_NAME);
        assert!(tool_name("prom", &long).unwrap_err().contains("longer than 64"));
        assert_eq!(
            tool_name("p", &"x".repeat(MAX_TOOL_NAME - 3)).unwrap().len(),
            MAX_TOOL_NAME
        );
    }

    #[test]
    fn server_names_are_lowercase_letters_digits_and_dashes() {
        assert!(check_server_name("prom-2").is_ok());
        for name in ["", "Prom", "prom_x", "prom x", "prom.x"] {
            assert!(check_server_name(name).is_err(), "{name:?}");
        }
    }

    #[test]
    fn long_descriptions_are_cut_at_a_character_boundary() {
        assert_eq!(bounded("short", 10), "short");
        let cut = bounded("ab中cd", 3);
        assert_eq!(cut, "ab [description truncated]");
    }
}
