//! What every session of a process shares: the profile's MCP servers, connected once, the
//! secret files to redact, checked once, and the variables commands must not see.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use zx_agent::agent::EventSink;
use zx_agent::extensions::Extension;
use zx_agent::mcp::{Server, ServerConfig};
use zx_agent::profile::{Builtin, Profile};
use zx_agent::redact::{MIN_SECRET_CHARS, check_secret_files, short_secret_files};

use super::{BaseArgs, TransportArgs, env_value};

/// Comma-separated secret files to redact, added to `--secret-file`; deployments set it, for
/// example to the token that the platform mounts for the agent's own account.
const SECRET_FILES_ENV: &str = "ZX_SECRET_FILES";

/// What every session of a process shares: the MCP servers, the secret files and the hidden
/// variables.
pub(crate) struct Shared {
    pub(super) mcp: McpTools,
    /// The files whose contents are redacted from tool output.
    pub(super) secret_files: Vec<PathBuf>,
    /// Variables that hold credentials besides the API key, such as IM tokens: commands do
    /// not inherit them, and their values are redacted like the key's.
    pub(super) hidden_env: Vec<String>,
}

impl Shared {
    /// Connect the MCP servers of the selected profile and check the secret files.
    ///
    /// A missing token, a server that cannot be reached in time, a tool it lacks or a secret
    /// file that cannot be used stops startup, like other configuration errors.
    pub(crate) async fn connect(base: &BaseArgs, default: Builtin, transport: &TransportArgs) -> Result<Self> {
        Ok(Self {
            mcp: connect_mcp(base, default, transport).await?,
            secret_files: secret_files(base)?,
            hidden_env: Vec::new(),
        })
    }

    /// Hide `names` from commands and redact their values: credentials besides the API key,
    /// such as the IM platforms' tokens of im.
    pub(crate) fn with_hidden_env(mut self, names: Vec<String>) -> Self {
        self.hidden_env = names;
        self
    }
}

/// What a mode's sessions are built from: its arguments, what the process shares, and where
/// traces go when an OTLP endpoint is configured.
pub(crate) struct ModeSettings<A> {
    pub(crate) args: A,
    pub(crate) shared: Shared,
    pub(crate) traces: Option<Arc<dyn EventSink>>,
}

/// The profile's MCP servers, connected once per process and shared by every session it
/// opens, as im does for each thread.
#[derive(Default)]
pub(super) struct McpTools {
    pub(super) servers: Vec<Arc<dyn Extension>>,
    /// Tools that need approval: those not in their server's `allowTools`, unless the
    /// profile approves every call.
    pub(super) ask: Vec<String>,
    /// Variables holding the servers' tokens: hidden from commands, values redacted.
    pub(super) token_env: Vec<String>,
}

/// Connect the MCP servers of the selected profile, printing one line per server on stderr,
/// without the token or the URL's path.
async fn connect_mcp(base: &BaseArgs, default: Builtin, transport: &TransportArgs) -> Result<McpTools> {
    let profile = Profile::load(base.profile.as_deref(), default)?;
    let mut configs = Vec::new();
    for server in &profile.mcp {
        let token = match &server.token_env {
            Some(name) => Some(
                env_value(name)
                    .with_context(|| format!("MCP server {}: {name} must hold its token (tokenEnv)", server.name))?,
            ),
            None => None,
        };
        configs.push(ServerConfig {
            token,
            call_timeout: Duration::from_secs(server.timeout_secs.unwrap_or(transport.tool_timeout_secs)),
            max_call_timeout: server.max_timeout_secs.map(Duration::from_secs),
            ..ServerConfig::new(server.name.as_str(), server.url.as_str(), server.tools.clone())
        });
    }
    // Each connection has its own bound, so the slowest server sets the startup time.
    let connected = futures::future::join_all(configs.into_iter().map(Server::connect)).await;
    let mut tools = McpTools::default();
    for (settings, server) in profile.mcp.iter().zip(connected) {
        let server = server.map_err(anyhow::Error::msg)?;
        let mut names = Vec::new();
        for (remote, name) in server.tool_names() {
            // With approveAll nothing asks: the container is the sandbox.
            if settings.allow_tools.contains(remote) || profile.rules.approve_all {
                names.push(name.clone());
            } else {
                names.push(format!("{name} (asks)"));
                tools.ask.push(name.clone());
            }
        }
        // The origin only: a path or query may hold a credential. `Server::connect` has parsed
        // the URL already, so the empty fallback is never shown.
        let origin = reqwest::Url::parse(&settings.url)
            .map(|url| url.origin().ascii_serialization())
            .unwrap_or_default();
        let plain = if settings.token_env.is_some() && origin.starts_with("http://") {
            "; the token is sent unencrypted"
        } else {
            ""
        };
        eprintln!(
            "[mcp] {}: {}; protocol {}; {origin}{plain}",
            settings.name,
            names.join(", "),
            server.protocol_version()
        );
        tools.token_env.extend(settings.token_env.iter().cloned());
        tools.servers.push(Arc::new(server));
    }
    Ok(tools)
}

/// The secret files to redact: `--secret-file` and `ZX_SECRET_FILES`, each checked at startup.
///
/// Both sources count: a file named at the command line must not drop the ones a deployment
/// registered in the environment. A misspelt path would leave a secret unredacted without a
/// word, so each must exist at startup, and every file must be one the redaction can match;
/// one that disappears later is skipped, and one that becomes unusable later withholds the
/// output (`RedactSecrets::with_secret_files`). The notice about short values describes the
/// configuration, which stays the same while the process runs, so it is said here, once.
fn secret_files(base: &BaseArgs) -> Result<Vec<PathBuf>> {
    let from_environment: Vec<PathBuf> = match std::env::var_os(SECRET_FILES_ENV) {
        Some(value) => value
            .to_str()
            .with_context(|| format!("{SECRET_FILES_ENV} is not valid UTF-8"))?
            .split(',')
            .map(PathBuf::from)
            .collect(),
        None => Vec::new(),
    };
    let secret_files: Vec<PathBuf> = from_environment
        .into_iter()
        .chain(base.secret_files.iter().cloned())
        .filter(|path| !path.as_os_str().is_empty())
        .collect();
    for path in &secret_files {
        std::fs::metadata(path).with_context(|| {
            format!(
                "cannot use secret file {} (--secret-file or ZX_SECRET_FILES)",
                path.display()
            )
        })?;
    }
    check_secret_files(&secret_files).map_err(|error| anyhow::anyhow!("{error} (--secret-file or ZX_SECRET_FILES)"))?;
    // Never the value itself: the notice goes to a terminal or a log.
    for file in short_secret_files(&secret_files) {
        eprintln!(
            "[secret] {}: the value is shorter than {MIN_SECRET_CHARS} characters; it is still redacted, and so is every other occurrence of the same text in tool output",
            file.display()
        );
    }
    Ok(secret_files)
}
