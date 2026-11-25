//! A client for external MCP servers that only lists and calls their tools, over Streamable
//! HTTP. The servers, and which of their tools the model gets, come from configuration;
//! whether a call may run is for the host's policies to decide, not for the server: its tool
//! annotations are only hints, unless the host trusts its read-only ones
//! ([`ServerConfig::trust_read_only`]).
//!
//! A server that goes down for a while, or restarts and loses its session, is called again
//! once it answers: a call in between fails, and when the server says the session is gone
//! (HTTP 404), rmcp opens a new one and sends the request again. The tools are listed once,
//! when connecting.
//!
//! A [`Server`] gives its tools ([`Server::tools`]) for an agent's
//! [`AgentConfig::tools`](zx_core::AgentConfig::tools).
//!
//! - `http`: the HTTP client a server's transport sends with, through its proxy

mod http;

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, HeaderName, HeaderValue};
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, CancelledNotificationParam, ClientCapabilities,
    ClientConfig, ClientRequest, ContentBlock, Implementation, ProgressNotificationParam, ProgressToken,
    ProtocolVersion, RequestId, ResourceContents, ServerResult,
};
use rmcp::service::{NotificationContext, Peer, PeerRequestOptions, RunningService, ServiceError};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ClientHandler, ClientLifecycleMode, RoleClient, serve_client_with_lifecycle};
use serde_json::{Map, Value};
use tokio::sync::mpsc;
use zx_core::tools::{Effects, Tool, ToolContext, ToolOutput};

use self::http::http_client;
use crate::ai::{Proxy, ToolDef, api_identifier};
use crate::images;
use crate::sync::lock;
use crate::text::prefix_noted;
use crate::tools::truncate::truncate_head_noted;
use crate::tools::unless_cancelled;

/// The longest tool name that OpenAI-compatible APIs accept: "a-z, A-Z, 0-9, or contain
/// underscores and dashes, with a maximum length of 64" (openai-openapi, `FunctionObject`).
pub const MAX_TOOL_NAME_CHARS: usize = 64;
/// Tool descriptions go with every request and are written by the server, so they are cut
/// to this size.
const MAX_DESCRIPTION_BYTES: usize = 4 * 1024;
/// Default bound for connecting and listing the tools.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Default bound for one tool call, without news from the server.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// Progress updates a call can hold before the server's notifications are dropped: a call
/// reads them as they come, and a burst beyond this only loses updates.
const MAX_QUEUED_PROGRESS: usize = 16;
/// Longest wait for a server to take the notice that a request is given up. The notice is a
/// courtesy that lets the transport close the request's stream; the call that needed the
/// answer is gone already.
const GIVE_UP_TIMEOUT: Duration = Duration::from_secs(10);
/// Headers the transport sets itself (Streamable HTTP), which configuration may not.
const TRANSPORT_HEADERS: [&str; 5] = [
    "accept",
    "content-type",
    "mcp-session-id",
    "mcp-protocol-version",
    "last-event-id",
];

/// One MCP server and the tools to take from it.
///
/// Build it with [`ServerConfig::new`] and struct update syntax. It holds the credential, so
/// it does not implement `Debug`.
pub struct ServerConfig {
    /// ASCII lowercase letters, digits and `-`. Tools reach the model as `<name>__<tool>`.
    pub name: String,
    /// The server's MCP endpoint, `http://` or `https://`.
    pub url: String,
    /// Sent as `Authorization: Bearer <token>` with every request; `None` sends none.
    pub token: Option<String>,
    /// More headers sent with every request, as name and value, such as a tenant or an API
    /// key. Not the transport's own (`Accept`, `Content-Type`, `Mcp-Session-Id`,
    /// `MCP-Protocol-Version`, `Last-Event-ID`), and no `Authorization` beside a `token`. A
    /// value may be a credential: errors name only its header.
    pub headers: Vec<(String, String)>,
    /// The server's tools to offer the model, by the names the server gives them. The others
    /// are left out, descriptions included. Not empty.
    pub tools: Vec<String>,
    /// Whether the server's `readOnlyHint` is trusted: a tool it annotates as read-only then
    /// declares [`Effects::ReadOnly`], which a host may let run without asking. Untrusted, the
    /// annotations are hints, and every tool declares side effects.
    pub trust_read_only: bool,
    /// Bound for connecting and listing the tools.
    pub connect_timeout: Duration,
    /// Longest a call may go without an answer or a progress notification: each
    /// notification starts it over (pi: the request timeout re-armed on progress). A call
    /// that keeps reporting progress is bounded by the run's timeout and cancellation.
    pub call_timeout: Duration,
    /// How requests reach the server: the environment's proxies by default.
    pub proxy: Proxy,
}

impl ServerConfig {
    /// The settings for `tools` of the server `name` at `url`: no token or other headers,
    /// annotations not trusted, the default timeouts ([`DEFAULT_CONNECT_TIMEOUT`],
    /// [`DEFAULT_CALL_TIMEOUT`]) and the environment's proxies.
    pub fn new(name: impl Into<String>, url: impl Into<String>, tools: Vec<String>) -> Self {
        Self {
            name: name.into(),
            url: url.into(),
            token: None,
            headers: Vec::new(),
            tools,
            trust_read_only: false,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            call_timeout: DEFAULT_CALL_TIMEOUT,
            proxy: Proxy::default(),
        }
    }

    /// Check the settings without connecting, and return the names the model calls the
    /// configured tools by, in their order: `<name>__<tool>`, with characters other than ASCII
    /// letters, digits, `_` and `-` replaced by `_`. [`Server::connect`] checks the same.
    ///
    /// Errors, each naming the server: a name that is not ASCII lowercase letters, digits and
    /// `-` (so that it ends at the first `__` of a tool's name); a URL that is not `http://` or
    /// `https://`; a header whose name or value is not valid, that is the transport's own,
    /// that is given twice, or that is `Authorization` beside a token; no tools; a name longer
    /// than model APIs accept ([`MAX_TOOL_NAME_CHARS`]); two tools that would get the same
    /// name.
    pub fn validate(&self) -> Result<Vec<String>, String> {
        self.check().map(|checked| checked.names)
    }

    /// What [`validate`](Self::validate) checks, with what it parsed.
    fn check(&self) -> Result<Checked, String> {
        let server = self.name.as_str();
        check_server_name(server)?;
        if self.url.trim().is_empty() {
            return Err(fault(server, "url is empty"));
        }
        let url = reqwest::Url::parse(&self.url).map_err(|error| fault(server, format!("invalid url: {error}")))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(fault(server, "the url must be http:// or https://"));
        }
        let headers = self.header_map()?;
        if self.tools.is_empty() {
            return Err(fault(server, "tools lists no tool"));
        }
        let mut names: Vec<String> = Vec::new();
        for tool in &self.tools {
            let name = tool_name(server, tool)?;
            if names.contains(&name) {
                return Err(fault(
                    server,
                    format!("tool {tool:?} would be named {name:?}, like another of its tools"),
                ));
            }
            names.push(name);
        }
        Ok(Checked {
            address: Address {
                urls: [self.url.clone(), url.as_str().to_string()],
                origin: url.origin().ascii_serialization(),
            },
            headers,
            names,
        })
    }

    /// The configured headers as the transport sends them, the values marked sensitive.
    /// Errors name the header and never its value.
    fn header_map(&self) -> Result<HashMap<HeaderName, HeaderValue>, String> {
        let server = self.name.as_str();
        let mut headers = HashMap::new();
        for (name, value) in &self.headers {
            let refused = |problem: &str| fault(server, format_args!("header {name:?} {problem}"));
            let parsed = HeaderName::from_bytes(name.as_bytes()).map_err(|_| refused("is not a valid header name"))?;
            if TRANSPORT_HEADERS.contains(&parsed.as_str()) {
                return Err(refused("is set by the transport"));
            }
            if parsed == AUTHORIZATION && self.token.is_some() {
                return Err(refused("is set by the token"));
            }
            let mut value =
                HeaderValue::from_str(value).map_err(|_| refused("has a value that is not a valid header value"))?;
            value.set_sensitive(true);
            if headers.insert(parsed, value).is_some() {
                return Err(refused("is given more than once"));
            }
        }
        Ok(headers)
    }
}

/// The settings of a server, checked.
struct Checked {
    address: Address,
    /// The configured headers, by lowercase name.
    headers: HashMap<HeaderName, HeaderValue>,
    /// The names the model calls its tools by, in configuration order.
    names: Vec<String>,
}

/// Where a server is, and what of it may be shown: the user, path or query of its URL may
/// hold a credential, and the HTTP client's errors write the whole URL.
#[derive(Clone)]
struct Address {
    /// The URL as configured, and as the HTTP client parsed and writes it.
    urls: [String; 2],
    /// The scheme, host and port of the URL.
    origin: String,
}

impl Address {
    /// `text` with the server's URL written as its origin.
    fn hide(&self, text: &str) -> String {
        // The longer form first, so that the other cannot leave a part of it behind.
        let mut urls: Vec<&String> = self.urls.iter().collect();
        urls.sort_by_key(|url| std::cmp::Reverse(url.len()));
        urls.into_iter()
            .fold(text.to_string(), |text, url| text.replace(url.as_str(), &self.origin))
    }
}

type Client = RunningService<RoleClient, Handler>;

/// A connected MCP server and the tools taken from it.
pub struct Server {
    origin: String,
    protocol_version: String,
    tools: Vec<Arc<dyn Tool>>,
    /// The names the model calls the tools by, in configuration order.
    names: Vec<String>,
}

impl Server {
    /// Connect, settle the protocol version, list the server's tools and keep the configured
    /// ones. The server is asked for `server/discover` first (protocol 2026-07-28) and, if it
    /// answers as an older server does, for the `initialize` handshake (2025-11-25).
    ///
    /// Errors, each naming the server: settings that [`ServerConfig::validate`] refuses, no
    /// answer within `connect_timeout`, a protocol or transport failure, and a configured tool
    /// the server does not offer. The token, the header values and the URL's user, path and
    /// query never appear in them.
    pub async fn connect(config: ServerConfig) -> Result<Self, String> {
        let checked = config.check()?;
        let (client, listed) = connect_and_list(&config, &checked).await?;
        // A completed handshake or discovery records the server's information, so the empty
        // fallback is not expected; it only affects what is reported.
        let protocol_version = client
            .peer_info()
            .map(|info| info.protocol_version.to_string())
            .unwrap_or_default();
        Self::with_tools(&config, checked, protocol_version, &listed, &Arc::new(client))
    }

    /// The server with the configured tools among those it `listed`, as tools for the model,
    /// which calls them by the `checked` names, one for each configured tool.
    fn with_tools(
        config: &ServerConfig,
        checked: Checked,
        protocol_version: String,
        listed: &[rmcp::model::Tool],
        client: &Arc<Client>,
    ) -> Result<Self, String> {
        let server = config.name.as_str();
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        for (remote, name) in config.tools.iter().zip(&checked.names) {
            let Some(offered) = listed.iter().find(|tool| tool.name.as_ref() == remote.as_str()) else {
                let offered: Vec<&str> = listed.iter().map(|tool| tool.name.as_ref()).collect();
                return Err(fault(
                    server,
                    format!("it has no tool named {remote:?}; it offers: {}", offered.join(", ")),
                ));
            };
            let read_only = offered
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.read_only_hint);
            tools.push(Arc::new(McpTool {
                definition: ToolDef {
                    name: name.clone(),
                    description: description(offered.description.as_deref()),
                    parameters: Value::Object(offered.input_schema.as_ref().clone()),
                },
                remote: remote.clone(),
                server: server.to_string(),
                address: checked.address.clone(),
                client: Arc::clone(client),
                timeout: config.call_timeout,
                effects: if config.trust_read_only && read_only == Some(true) {
                    Effects::ReadOnly
                } else {
                    Effects::SideEffects
                },
            }));
        }
        Ok(Self {
            origin: checked.address.origin,
            protocol_version,
            tools,
            names: checked.names,
        })
    }

    /// The scheme, host and port of the server's URL, such as `https://mcp.example.com`: what
    /// may be shown of it, since its user, path or query may hold a credential.
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// The protocol version settled with the server, such as `2026-07-28`.
    pub fn protocol_version(&self) -> &str {
        &self.protocol_version
    }

    /// The names the model calls the tools by, in configuration order.
    pub fn tool_names(&self) -> &[String] {
        &self.names
    }

    /// The configured tools, in configuration order, as an agent offers them. They share the
    /// connection, which stays open while one of them is kept.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.clone()
    }
}

/// An error of `server`, named first so that every message says which server failed.
fn fault(server: &str, message: impl std::fmt::Display) -> String {
    format!("MCP server {server}: {message}")
}

/// What went wrong with a request: a transport's own error without rmcp's wrapping, which
/// names its internal types, and other errors as rmcp writes them.
fn service_error(error: &ServiceError) -> String {
    match error {
        ServiceError::TransportSend(sent) => sent.error.to_string(),
        other => other.to_string(),
    }
}

/// Connect to the server within its connect timeout, sending the `checked` headers with every
/// request, and list its tools.
async fn connect_and_list(
    config: &ServerConfig,
    checked: &Checked,
) -> Result<(Client, Vec<rmcp::model::Tool>), String> {
    let server = config.name.as_str();
    // rmcp starts a new session and sends the request again when the server answers 404, as
    // it does once it lost the session (`reinit_on_expired_session`, on by default).
    let mut transport =
        StreamableHttpClientTransportConfig::with_uri(config.url.as_str()).custom_headers(checked.headers.clone());
    if let Some(token) = &config.token {
        transport = transport.auth_header(token.as_str());
    }
    let client = http_client(&config.proxy).map_err(|error| fault(server, error))?;
    let transport = StreamableHttpClientTransport::with_client(client, transport);
    let lifecycle = ClientLifecycleMode::Auto {
        preferred_versions: vec![ProtocolVersion::LATEST],
        legacy_version: Some(ProtocolVersion::LATEST_WITH_INITIALIZE),
    };
    let connecting = async {
        let client = serve_client_with_lifecycle(Handler::new(), transport, lifecycle)
            .await
            .map_err(|error| error.to_string())?;
        let listed = client.list_all_tools().await.map_err(|error| service_error(&error))?;
        Ok::<_, String>((client, listed))
    };
    match tokio::time::timeout(config.connect_timeout, connecting).await {
        Ok(Ok(connected)) => Ok(connected),
        Ok(Err(error)) => Err(fault(server, checked.address.hide(&error))),
        Err(_) => Err(fault(server, format!("no answer within {:?}", config.connect_timeout))),
    }
}

/// The name the model calls a server's tool by: `<server>__<tool>`, with characters other than
/// ASCII letters, digits, `_` and `-` replaced by `_`. MCP allows tool names of up to 128
/// characters, including `.`; model APIs allow fewer (`MAX_TOOL_NAME_CHARS`). A server name has no
/// `_`, so the first `__` ends it. Errors when the name would be too long.
fn tool_name(server: &str, tool: &str) -> Result<String, String> {
    let name = format!("{server}__{}", api_identifier(tool));
    if name.len() > MAX_TOOL_NAME_CHARS {
        return Err(fault(
            server,
            format_args!("tool name {name:?} is longer than {MAX_TOOL_NAME_CHARS} characters"),
        ));
    }
    Ok(name)
}

/// A server name is ASCII lowercase letters, digits and `-`, so that it ends at the first `__`
/// of a tool name.
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

/// A tool's description for the model, cut to `MAX_DESCRIPTION_BYTES`; a server need not
/// give one.
fn description(offered: Option<&str>) -> String {
    prefix_noted(offered.unwrap_or_default(), MAX_DESCRIPTION_BYTES, |_, _| {
        " [description truncated]".to_string()
    })
}

/// Identifies zx-agent and declares no optional capabilities: only tools are used. Progress
/// notifications are handed to the call they belong to, by the token rmcp gave its request.
struct Handler {
    info: ClientConfig,
    progress: Mutex<HashMap<ProgressToken, mpsc::Sender<ProgressNotificationParam>>>,
}

impl Handler {
    fn new() -> Self {
        Self {
            info: ClientConfig::new(
                ClientCapabilities::default(),
                Implementation::new("zx-agent", env!("CARGO_PKG_VERSION")),
            ),
            progress: Mutex::new(HashMap::new()),
        }
    }

    /// The progress notifications carrying `token`, until the updates are dropped.
    fn subscribe(&self, token: ProgressToken) -> ProgressUpdates<'_> {
        let (sender, receiver) = mpsc::channel(MAX_QUEUED_PROGRESS);
        lock(&self.progress).insert(token.clone(), sender);
        ProgressUpdates {
            handler: self,
            token,
            receiver,
        }
    }
}

impl ClientHandler for Handler {
    fn get_info(&self) -> ClientConfig {
        self.info.clone()
    }

    fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + Send + '_ {
        let sender = lock(&self.progress).get(&params.progress_token).cloned();
        if let Some(sender) = sender {
            // A notification for a call that ended, or one more than the call's queue holds,
            // is dropped. Each is handled by a task of its own, and tasks waiting for room
            // would pile up as fast as a server sends.
            sender.try_send(params).ok();
        }
        std::future::ready(())
    }
}

/// The progress of one call; dropping it stops the handler from keeping its notifications.
struct ProgressUpdates<'a> {
    handler: &'a Handler,
    token: ProgressToken,
    receiver: mpsc::Receiver<ProgressNotificationParam>,
}

impl Drop for ProgressUpdates<'_> {
    fn drop(&mut self) {
        lock(&self.handler.progress).remove(&self.token);
    }
}

/// One of a server's tools, as the model sees it.
struct McpTool {
    definition: ToolDef,
    /// The server's name for the tool.
    remote: String,
    server: String,
    address: Address,
    client: Arc<Client>,
    /// Re-armed by every progress notification.
    timeout: Duration,
    /// Side effects, unless the host trusts the server's annotation that the tool only reads.
    effects: Effects,
}

impl McpTool {
    /// The error of a failed call, as [`service_error`] writes it, bounded like a result.
    fn failed(&self, error: &ServiceError) -> String {
        let text = self.address.hide(&service_error(error));
        fault(
            &self.server,
            format_args!("{} failed: {}", self.remote, truncate_head_noted(&text, "error")),
        )
    }

    /// The error of a call that went `bound` without an answer or progress.
    fn no_answer_within(&self, bound: Duration) -> String {
        fault(
            &self.server,
            format_args!("{} did not answer within {bound:?}", self.remote),
        )
    }

    /// Send the call and wait for its result, as [`Tool::execute`] describes for this tool.
    async fn call(&self, arguments: Map<String, Value>, ctx: &ToolContext) -> Result<CallToolResult, String> {
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(
            CallToolRequestParams::new(self.remote.clone()).with_arguments(arguments),
        ));
        let options = PeerRequestOptions::with_timeout(self.timeout).reset_timeout_on_progress();
        let peer = self.client.peer();
        let handle = peer
            .send_cancellable_request(request, options)
            .await
            .map_err(|error| self.failed(&error))?;
        let unanswered = Unanswered {
            peer,
            request_id: Some(handle.id.clone()),
        };
        // rmcp gives every request a progress token; the server's notifications come back
        // with it, and the dispatcher hands them to this subscription.
        let mut updates = self.client.service().subscribe(handle.progress_token.clone());
        let answered = handle.await_response();
        tokio::pin!(answered);
        let answer = loop {
            tokio::select! {
                answer = &mut answered => break answer,
                Some(update) = updates.receiver.recv() => {
                    // A full progress queue only loses this update.
                    ctx.report_progress(progress_text(&update));
                }
                () = ctx.cancel.cancelled() => {
                    // Dropping `unanswered` tells the server.
                    return Err(fault(
                        &self.server,
                        format_args!("the call to {} was cancelled", self.remote),
                    ));
                }
            }
        };
        // Answered, or given up by rmcp, which tells the server itself when a call times out.
        unanswered.settle();
        match answer {
            Ok(ServerResult::CallToolResult(result)) => Ok(result),
            Ok(_) => Err(fault(
                &self.server,
                format_args!("{} answered with a result of another kind", self.remote),
            )),
            Err(ServiceError::Timeout { timeout }) => Err(self.no_answer_within(timeout)),
            Err(error) => Err(self.failed(&error)),
        }
    }
}

/// A request that was sent and has no answer yet. Dropped like that, it tells the server
/// that the request is given up: when a call sees its token cancelled, and when its future is
/// dropped, as the loop drops the calls of a cancelled run without their noticing.
struct Unanswered<'a> {
    peer: &'a Peer<RoleClient>,
    /// `None` once the request needs no giving up.
    request_id: Option<RequestId>,
}

impl Unanswered<'_> {
    /// The request ended by itself: there is nothing to give up.
    fn settle(mut self) {
        self.request_id = None;
    }
}

impl Drop for Unanswered<'_> {
    fn drop(&mut self) {
        // Spawning a Tokio task needs a runtime; without one nothing can be sent anyway.
        let (Some(request_id), Ok(_)) = (self.request_id.take(), tokio::runtime::Handle::try_current()) else {
            return;
        };
        let peer = self.peer.clone();
        // Not awaited: the call that would wait for it is gone. The task ends when the
        // notification is sent, the connection turns out to be closed, or the wait times out.
        tokio::spawn(async move { give_up(&peer, request_id).await });
    }
}

/// Tell the server that the agent gave up the request `request_id`, waiting at most
/// [`GIVE_UP_TIMEOUT`]. The notification is what makes the transport close the request's
/// stream; one that cannot be sent in time leaves nothing more to do, so its result is not
/// needed.
async fn give_up(peer: &Peer<RoleClient>, request_id: RequestId) {
    let notice = peer.notify_cancelled(CancelledNotificationParam::new(
        Some(request_id),
        Some("cancelled by the agent".to_string()),
    ));
    let _ = tokio::time::timeout(GIVE_UP_TIMEOUT, notice).await;
}

/// The tool output of a result: its text, bounded, its images fitted to the model APIs'
/// limits, and whether the server reported an error. An image that cannot be fitted is a
/// line of the text that says why.
async fn output_of(result: &CallToolResult) -> ToolOutput {
    let mut images = Vec::new();
    let mut refusals = Vec::new();
    for block in &result.content {
        if let ContentBlock::Image(image) = block {
            match images::fit_base64(&image.data).await {
                Ok(fitted) => {
                    images.push(fitted.image);
                    refusals.push(None);
                }
                Err(reason) => refusals.push(Some(reason)),
            }
        }
    }
    let text = render(result, &refusals);
    ToolOutput {
        // A result of images alone has no text to bound.
        content: if text.is_empty() {
            text
        } else {
            truncate_head_noted(&text, "result")
        },
        images,
        // Absent means false.
        is_error: result.is_error == Some(true),
        ..ToolOutput::default()
    }
}

/// The line a progress notification contributes (pi: `message`, or `Progress <n>/<total>`).
fn progress_text(update: &ProgressNotificationParam) -> String {
    match (&update.message, update.total) {
        (Some(message), _) if !message.is_empty() => message.clone(),
        (_, Some(total)) => format!("progress {}/{total}", update.progress),
        (_, None) => format!("progress {}", update.progress),
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.definition.name
    }

    fn definition(&self) -> ToolDef {
        self.definition.clone()
    }

    /// Side effects, so that a call that was interrupted is reported as possibly having
    /// changed state, unless the host trusts the server's annotation that the tool only reads.
    fn effects(&self) -> Effects {
        self.effects
    }

    /// Progress notifications of the call reach `ctx` as progress updates and re-arm the
    /// timeout. Stops waiting on `ctx.cancel` or after `timeout` without an answer or
    /// progress. A call that is cancelled, or dropped before its answer as the loop drops
    /// the calls of a cancelled run, ends the request on the server's side with
    /// `notifications/cancelled`, which also closes the request's stream.
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, String> {
        let Value::Object(arguments) = args else {
            return Err("the arguments must be a JSON object".to_string());
        };
        let result = self.call(arguments, ctx).await?;
        unless_cancelled(ctx, output_of(&result)).await.ok_or_else(|| {
            format!(
                "the call to {} was cancelled while its images were prepared",
                self.remote
            )
        })
    }
}

/// The text the model gets: text blocks as they are, a line for content it cannot read, and
/// the structured result when there is no other content. Images go beside the text, except
/// those whose fitting failed: `refusals` holds, image by image, why or `None`.
fn render(result: &CallToolResult, refusals: &[Option<String>]) -> String {
    let mut refusals = refusals.iter();
    let mut parts: Vec<String> = Vec::new();
    for block in &result.content {
        parts.push(match block {
            ContentBlock::Text(text) => text.text.clone(),
            ContentBlock::Image(image) => match refusals.next() {
                Some(Some(reason)) => format!("[image {} omitted: {reason}]", image.mime_type),
                _ => continue,
            },
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
    if parts.is_empty() && !has_images(result) {
        return "(no content)".to_string();
    }
    parts.join("\n")
}

/// Whether `result` holds an image.
fn has_images(result: &CallToolResult) -> bool {
    result
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::Image(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_are_prefixed_and_limited_to_what_model_apis_accept() {
        assert_eq!(tool_name("prom", "query").unwrap(), "prom__query");
        assert_eq!(tool_name("prom", "range.query v2").unwrap(), "prom__range_query_v2");
        assert_eq!(tool_name("prom", "中文").unwrap(), "prom____");
        let long = "x".repeat(MAX_TOOL_NAME_CHARS);
        assert!(tool_name("prom", &long).unwrap_err().contains("longer than 64"));
        assert_eq!(
            tool_name("p", &"x".repeat(MAX_TOOL_NAME_CHARS - 3)).unwrap().len(),
            MAX_TOOL_NAME_CHARS
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
    fn settings_are_checked_without_connecting_and_name_the_tools_for_the_model() {
        let tools = |names: &[&str]| names.iter().map(ToString::to_string).collect::<Vec<_>>();
        let config = ServerConfig::new("prom", "https://prom.example/mcp", tools(&["query", "range.query"]));
        assert_eq!(config.validate().unwrap(), ["prom__query", "prom__range_query"]);
        for (config, expected) in [
            (
                ServerConfig::new("Prom", "https://x", tools(&["q"])),
                "must be ASCII lowercase",
            ),
            (ServerConfig::new("prom", " ", tools(&["q"])), "url is empty"),
            (ServerConfig::new("prom", "not a url", tools(&["q"])), "invalid url"),
            (
                ServerConfig::new("prom", "ftp://x", tools(&["q"])),
                "must be http:// or https://",
            ),
            (
                ServerConfig::new("prom", "https://x", tools(&[])),
                "tools lists no tool",
            ),
            (
                ServerConfig::new("prom", "https://x", tools(&["a.b", "a_b"])),
                "tool \"a_b\" would be named \"prom__a_b\", like another of its tools",
            ),
            (
                ServerConfig::new("prom", "https://x", tools(&[&"x".repeat(MAX_TOOL_NAME_CHARS)])),
                "longer than 64",
            ),
        ] {
            let error = config.validate().unwrap_err();
            assert!(error.starts_with("MCP server ") && error.contains(expected), "{error}");
            assert!(error.contains(&config.name), "{error}");
        }
    }

    #[test]
    fn headers_are_checked_and_their_values_never_shown() {
        let secret = "secret-1234";
        let headers = |pairs: &[(&str, &str)]| ServerConfig {
            headers: pairs
                .iter()
                .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
                .collect(),
            ..ServerConfig::new("prom", "https://x", vec!["q".to_string()])
        };
        assert!(
            headers(&[("X-Scope-OrgID", "tenant-a"), ("X-Api-Key", secret)])
                .validate()
                .is_ok()
        );
        // Without a token, Authorization is a header like any other.
        assert!(headers(&[("Authorization", "Basic abc")]).validate().is_ok());
        for (config, expected) in [
            (headers(&[("X Api", secret)]), "is not a valid header name"),
            (headers(&[("Content-Type", secret)]), "is set by the transport"),
            (headers(&[("mcp-session-id", secret)]), "is set by the transport"),
            (
                headers(&[("X-Api-Key", &format!("{secret}\nX-Other: 1"))]),
                "has a value that is not a valid header value",
            ),
            (
                ServerConfig {
                    token: Some("token".to_string()),
                    ..headers(&[("authorization", secret)])
                },
                "is set by the token",
            ),
            (
                headers(&[("X-Api-Key", secret), ("x-api-key", secret)]),
                "is given more than once",
            ),
        ] {
            let error = config.validate().unwrap_err();
            assert!(
                error.starts_with("MCP server prom: header ") && error.contains(expected),
                "{error}"
            );
            assert!(!error.contains(secret), "{error}");
        }
    }

    #[test]
    fn errors_show_the_origin_of_the_url_in_its_place() {
        let url = "https://user:pw@Prom.example/mcp?key=s3cret";
        let address = ServerConfig::new("prom", url, vec!["q".to_string()])
            .check()
            .unwrap()
            .address;
        // The HTTP client writes the host in lowercase.
        let written = "error sending request for url (https://user:pw@prom.example/mcp?key=s3cret)";
        assert_eq!(
            address.hide(written),
            "error sending request for url (https://prom.example)"
        );
        assert_eq!(address.hide(&format!("at {url}")), "at https://prom.example");
    }

    #[test]
    fn long_descriptions_are_cut_and_a_missing_one_is_empty() {
        assert_eq!(description(Some("short")), "short");
        assert_eq!(description(None), "");
        let cut = description(Some(&"界".repeat(MAX_DESCRIPTION_BYTES)));
        assert!(
            cut.ends_with("界 [description truncated]"),
            "{}",
            &cut[cut.len() - 40..]
        );
        assert!(cut.len() <= MAX_DESCRIPTION_BYTES + " [description truncated]".len());
    }

    #[test]
    fn progress_lines_prefer_the_servers_message() {
        let update = |message: Option<&str>, progress: f64, total: Option<f64>| {
            let token = ProgressToken(rmcp::model::NumberOrString::Number(1));
            let mut update = ProgressNotificationParam::new(token, progress);
            if let Some(total) = total {
                update = update.with_total(total);
            }
            if let Some(message) = message {
                update = update.with_message(message);
            }
            update
        };
        assert_eq!(progress_text(&update(Some("step 1"), 1.0, Some(3.0))), "step 1");
        assert_eq!(progress_text(&update(Some(""), 2.0, Some(3.0))), "progress 2/3");
        assert_eq!(progress_text(&update(None, 2.5, None)), "progress 2.5");
    }
}
