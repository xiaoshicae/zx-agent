//! `config.toml`: where and with what an agent runs. The file is optional, and every key in
//! it is: what it leaves out keeps the built-in default. What a host and the environment
//! variables say over it is laid on by the assembly (`crate::assembly`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use zx_agent::ai::{ModelCost, ModelMeta, Proxies, Proxy, Reasoning};
use zx_agent::builtin::executor::{Access, ContainerConfig, InvalidContainerConfig};
use zx_agent::builtin::mcp::ServerConfig;
use zx_agent::builtin::output_store::{
    InvalidOutputStoreConfig, MAX_FILE_BYTES, MIN_THRESHOLD_BYTES, OutputStoreConfig,
};
use zx_ai_http::client::check_proxy;

use super::placeholders::{Credentials, Holds, carries_user, expand_table};
use super::provider::ProviderSettings;
use super::{parsed, read_text};

/// Where long tool results are stored unless `toolOutput.directory` says otherwise: relative
/// to the working directory of each session.
const DEFAULT_TOOL_OUTPUT_DIRECTORY: &str = ".zx-agent/outputs";

/// Seconds a bash command may run, and an MCP call wait, unless `toolTimeoutSecs` says
/// otherwise.
const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 120;

/// A day, the unit of `toolOutput.maxAgeDays`.
const SECONDS_PER_DAY: u64 = 24 * 60 * 60;

/// The built-in executor that runs commands on this machine, as the agent's user: the one
/// config.toml means unless `execution` names another.
pub const LOCAL: &str = "local";

/// The built-in executor that runs commands in a Docker container started for each session,
/// with the settings of [`ContainerSettings`].
pub const CONTAINER: &str = "container";

/// Container settings for `execution = "container"`, the `[container]` table; unset values use
/// the executor defaults.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct ContainerSettings {
    /// Required in container mode; the image must already be present locally.
    pub image: Option<String>,
    /// `uid:gid` that commands run as.
    pub user: Option<String>,
    /// Memory limit in Docker's syntax, such as `1g`.
    pub memory: Option<String>,
    /// Maximum number of processes in the container.
    pub pids_limit: Option<u32>,
    /// Docker network, such as `bridge` (the executor's default) or `none`.
    pub network: Option<String>,
    /// Whether commands may change the workspace; writable by default.
    pub workspace: Option<Access>,
    /// Whether commands may change the container's own files; writable by default.
    pub root: Option<Access>,
}

/// An external MCP server whose listed tools the model may call, connected when the host
/// starts. Its tools ask a person, and are refused where no one can answer, unless the
/// agent's `permissions.allow` lists them, or the server is trusted to say which only read.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct McpServer {
    /// ASCII lowercase letters, digits and `-`. Its tools reach the model as `<name>__<tool>`.
    pub name: String,
    /// The server's MCP endpoint (Streamable HTTP).
    pub url: String,
    /// The token sent as `Authorization: Bearer`, as it is; unset sends none. A credential:
    /// its value is redacted from tool output, whether config.toml gave it or a host did.
    /// The variables it names in config.toml are hidden from commands; a host that takes
    /// it from a variable itself hides that variable
    /// ([`Application::add_credentials`](crate::Application::add_credentials)).
    pub token: Option<String>,
    /// Headers sent with every request, by name, as they are. In config.toml, a value that
    /// names a variable is a credential, as the token is, and one written as it is, is not.
    /// A host that sets a credential here names it among [`Config::credentials`].
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// The server's tools to offer the model, by the names the server gives them.
    pub tools: Vec<String>,
    /// Whether the server's `readOnlyHint` is trusted: a tool it annotates as read-only runs
    /// without asking, unless the agent's `permissions.ask` lists it, and is treated as having
    /// no side effects.
    #[serde(default)]
    pub trust_read_only: bool,
    /// Longest a call may go without an answer or a progress notification, in seconds;
    /// unset uses `toolTimeoutSecs`. A call that keeps reporting progress is bounded by the
    /// run's timeout and cancellation.
    pub timeout_secs: Option<u64>,
}

impl McpServer {
    /// The settings the server is connected with: every key of its entry, its calls bounded
    /// by `timeoutSecs` or else by `tool_timeout`, and its requests sent through `proxy`.
    fn server_config(&self, tool_timeout: Duration, proxy: &Proxy) -> ServerConfig {
        let Self {
            name,
            url,
            token,
            headers,
            tools,
            trust_read_only,
            timeout_secs,
        } = self;
        ServerConfig {
            token: token.clone(),
            headers: headers.clone().into_iter().collect(),
            trust_read_only: *trust_read_only,
            call_timeout: timeout_secs.map_or(tool_timeout, Duration::from_secs),
            proxy: proxy.clone(),
            ..ServerConfig::new(name.as_str(), url.as_str(), tools.clone())
        }
    }
}

/// `config.toml`, checked: the environment every agent of this user runs in. One a host
/// builds itself holds final values: only the reading of the file replaces `${NAME}`
/// placeholders, but for [`ProviderSettings::api_key`], which is expanded wherever it comes
/// from.
#[derive(Debug)]
pub struct Config {
    /// Where the file is, or would be; `None` without a home directory.
    pub path: Option<PathBuf>,
    /// The model ID, unless the host names one ([`Overrides::model`](crate::Overrides::model)),
    /// or `ZX_MODEL` does where the environment is read
    /// ([`Application::from_environment`](crate::Application::from_environment)).
    pub model: Option<String>,
    /// How hard a reasoning model thinks.
    pub reasoning: Option<Reasoning>,
    /// Output token limit per response.
    pub max_tokens: Option<u32>,
    /// Default bash command timeout, and that of MCP calls, in seconds.
    pub tool_timeout_secs: Option<u64>,
    /// How long a tool call waits for a person's approval, in seconds; an unanswered request
    /// is denied.
    pub approval_timeout_secs: Option<u64>,
    /// Files holding secrets to redact from tool output, or directories of such files;
    /// absolute after load.
    pub secret_files: Vec<PathBuf>,
    /// How the model is reached.
    pub provider: ProviderSettings,
    /// Where the tools that run commands and touch files run: the executor, by the name it is
    /// registered under: [`LOCAL`], [`CONTAINER`], or one a host registers
    /// ([`Application::register_executor`](crate::Application::register_executor)).
    pub execution: String,
    /// Used when `execution` is [`CONTAINER`].
    pub container: ContainerSettings,
    /// Where long tool results are saved when read_output is enabled.
    pub tool_output: OutputStoreConfig,
    /// What is known about models, by ID (`[[models]]`): the context window and prices.
    pub models: HashMap<String, ModelMeta>,
    /// External MCP servers.
    pub mcp: Vec<McpServer>,
    /// How the program's own connections reach the network: those to the model, the MCP
    /// servers, the chat platforms and the trace collector. `[proxy]` gives the proxies,
    /// which replace the environment's; without it, the environment's apply as the HTTP
    /// client reads them. The commands an agent runs see the environment as it is.
    pub proxy: Proxy,
    /// The credentials the configuration holds, but for the API key, which the assembly
    /// expands: commands do not inherit their variables, and their values are redacted from
    /// tool output. The reading of config.toml lists them: the MCP servers' tokens, the
    /// headers that name variables and the proxies that carry a user or password, with the
    /// variables they name. A host that builds the configuration lists what its values do
    /// not tell: the variables it took a credential from, and a header that carries one. The
    /// tokens and such proxies are redacted whether listed or not.
    pub credentials: Credentials,
}

impl Default for Config {
    /// What a missing file means: the built-in defaults, and no file.
    fn default() -> Self {
        Self {
            path: None,
            model: None,
            reasoning: None,
            max_tokens: None,
            tool_timeout_secs: None,
            approval_timeout_secs: None,
            secret_files: Vec::new(),
            provider: ProviderSettings::default(),
            execution: LOCAL.to_string(),
            container: ContainerSettings::default(),
            tool_output: OutputStoreConfig::new(DEFAULT_TOOL_OUTPUT_DIRECTORY),
            models: HashMap::new(),
            mcp: Vec::new(),
            proxy: Proxy::default(),
            credentials: Credentials::default(),
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct RawConfig {
    model: Option<String>,
    reasoning: Option<String>,
    max_tokens: Option<u32>,
    tool_timeout_secs: Option<u64>,
    approval_timeout_secs: Option<u64>,
    secret_files: Vec<PathBuf>,
    /// The provider's table, which the provider reads but for the keys zx-app takes out.
    provider: toml::Table,
    execution: Option<String>,
    container: ContainerSettings,
    tool_output: RawToolOutput,
    models: Vec<RawModel>,
    mcp: Vec<McpServer>,
    /// `None` without a `[proxy]` table, which leaves the proxies to the environment.
    proxy: Option<RawProxy>,
}

/// `[proxy]`: the proxies of the program's own connections, in place of the environment's.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct RawProxy {
    http: Option<String>,
    https: Option<String>,
    no_proxy: Vec<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct RawToolOutput {
    directory: Option<PathBuf>,
    threshold_bytes: Option<usize>,
    max_file_bytes: Option<usize>,
    max_age_days: Option<u32>,
}

/// One `[[models]]` entry.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawModel {
    id: String,
    context_window: Option<u64>,
    cost: Option<ModelCost>,
    /// What the model takes in; text alone when absent (pi: `input`, `["text"]` by default).
    input: Option<Vec<ModelInput>>,
}

/// One kind of input a `[[models]]` entry says the model takes.
#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ModelInput {
    Text,
    Image,
}

impl Config {
    /// Read and check config.toml in the zx-agent home ([`home`](super::home)); without a
    /// home, the built-in defaults. The file
    /// [`Application::from_environment`](crate::Application::from_environment) reads, for
    /// what a host starts before it: its chat platforms and the trace export, which send
    /// through [`proxy`](Self::proxy).
    pub fn from_environment() -> Result<Self> {
        match super::home()? {
            Some(home) => Self::load(&home.join(super::CONFIG_FILE)),
            None => Ok(Self::default()),
        }
    }

    /// Read and check the file at `path`; a file that does not exist is an empty one. Paths in
    /// the file are relative to its directory.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            _ => Some(read_text(path)?),
        };
        Self::parse(text.as_deref(), path).with_context(|| format!("invalid {}", path.display()))
    }

    /// The configuration in `text`, the file at `path`; `None` is an empty file. Its
    /// placeholders are replaced.
    fn parse(text: Option<&str>, path: &Path) -> Result<Self> {
        let (raw, credentials) = match text {
            Some(text) => {
                // Read as written first, so that an error points at its line.
                toml::from_str::<RawConfig>(text)?;
                let mut table: toml::Table = toml::from_str(text)?;
                let credentials = expand_table(&mut table, &holds)?;
                (toml::Value::Table(table).try_into()?, credentials)
            }
            None => (RawConfig::default(), Credentials::default()),
        };
        // A file path always has a parent; the root stands in for one that has none.
        let parent = path.parent().unwrap_or_else(|| Path::new("/"));
        let mut config = raw.into_config(path, parent)?;
        config.credentials = credentials;
        config.validate()?;
        Ok(config)
    }

    /// How long a bash command may run, and a call of an MCP server without `timeoutSecs`
    /// wait: `toolTimeoutSecs`, or its default.
    pub(crate) fn tool_timeout(&self) -> Duration {
        Duration::from_secs(self.tool_timeout_secs.unwrap_or(DEFAULT_TOOL_TIMEOUT_SECS))
    }

    /// The settings each `[[mcp]]` server is connected with, in the order of the entries:
    /// what its entry says, its calls bounded by [`tool_timeout`](Self::tool_timeout) where
    /// it gives no `timeoutSecs`, and its requests sent through `[proxy]`.
    pub(crate) fn mcp_servers(&self) -> impl Iterator<Item = ServerConfig> + '_ {
        let tool_timeout = self.tool_timeout();
        self.mcp
            .iter()
            .map(move |server| server.server_config(tool_timeout, &self.proxy))
    }

    /// The names the model calls the configured MCP tools by, checking the `[[mcp]]` entries:
    /// the settings they are connected with as [`ServerConfig::validate`] does, whose errors
    /// name the server and its fields, and what only config.toml can say: a `timeoutSecs` of
    /// zero by its key, and two entries of one name.
    pub(crate) fn mcp_tool_names(&self) -> Result<Vec<String>> {
        let mut servers = HashSet::new();
        let mut names = Vec::new();
        for (server, config) in self.mcp.iter().zip(self.mcp_servers()) {
            let name = server.name.as_str();
            // Before the settings are checked, which would name the field they have for it.
            ensure!(
                server.timeout_secs != Some(0),
                "MCP server {name}: timeoutSecs must be greater than zero"
            );
            names.extend(config.validate().map_err(anyhow::Error::msg)?);
            ensure!(servers.insert(name), "more than one MCP server is named {name:?}");
        }
        Ok(names)
    }

    /// The values that are credentials by what they are, whether a file gave them or a host:
    /// the token of each MCP server, and each proxy that carries a user or password. The
    /// reading of a file finds them too, with the variables they name and the headers that
    /// name one, which only the file as written tells.
    pub(crate) fn credential_values(&self) -> Vec<String> {
        let tokens = self.mcp.iter().filter_map(|server| server.token.clone());
        let proxies = match &self.proxy {
            Proxy::Through(Proxies {
                http,
                https,
                no_proxy: _,
            }) => [http.clone(), https.clone()],
            Proxy::FromEnvironment | Proxy::Direct => [None, None],
        };
        tokens
            .chain(proxies.into_iter().flatten().filter(|url| carries_user(url)))
            .collect()
    }

    /// Check every value: what [`load`](Self::load) checks once it has read the file, and what
    /// a configuration a host builds itself must pass as well before anything runs with it.
    /// Errors name the key of config.toml that holds the value.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.model.as_deref().is_none_or(|model| !model.trim().is_empty()),
            "model must not be empty"
        );
        for (key, value) in [
            ("maxTokens", self.max_tokens.map(u64::from)),
            ("toolTimeoutSecs", self.tool_timeout_secs),
            ("approvalTimeoutSecs", self.approval_timeout_secs),
        ] {
            ensure!(value != Some(0), "{key} must be greater than zero");
        }
        ensure!(!self.execution.trim().is_empty(), "execution must not be empty");
        self.provider.validate(self.reasoning)?;
        for (id, meta) in &self.models {
            check_model(id, meta)?;
        }
        self.tool_output.validate().map_err(|error| tool_output_error(&error))?;
        // Without an image no container starts, and the rest of [container] is not used.
        if self.execution == CONTAINER || self.container.image.is_some() {
            self.container.container_config()?;
        }
        self.mcp_tool_names()?;
        if let Proxy::Through(proxies) = &self.proxy {
            for (key, url) in [("proxy.http", &proxies.http), ("proxy.https", &proxies.https)] {
                if let Some(url) = url {
                    check_proxy(url).map_err(|error| anyhow::anyhow!("{key} {error}"))?;
                }
            }
        }
        Ok(())
    }
}

impl ContainerSettings {
    /// The settings of the containers commands run in: these over the executor's defaults,
    /// checked. Errors name the keys of config.toml, as for a missing `container.image`.
    pub(crate) fn container_config(&self) -> Result<ContainerConfig> {
        let Self {
            image,
            user,
            memory,
            pids_limit,
            network,
            workspace,
            root,
        } = self;
        let Some(image) = image else {
            bail!("execution = \"container\" needs container.image");
        };
        let mut config = ContainerConfig::new(image.as_str());
        if let Some(user) = user {
            config.user.clone_from(user);
        }
        if let Some(memory) = memory {
            config.memory.clone_from(memory);
        }
        if let Some(limit) = *pids_limit {
            config.pids_limit = limit;
        }
        if let Some(network) = network {
            config.network = network.parse().map_err(container_error)?;
        }
        if let Some(access) = *workspace {
            config.workspace_access = access;
        }
        if let Some(access) = *root {
            config.root_access = access;
        }
        config.validate().map_err(container_error)?;
        Ok(config)
    }
}

/// A container setting the executor refuses, said with its key in config.toml.
fn container_error(error: InvalidContainerConfig) -> anyhow::Error {
    let message = match error {
        InvalidContainerConfig::EmptyImage => "container.image must not be empty".to_string(),
        InvalidContainerConfig::EmptyUser => "container.user must not be empty".to_string(),
        InvalidContainerConfig::RootUser(user) => {
            format!("container.user {user:?} is root; commands must run as an unprivileged user")
        }
        InvalidContainerConfig::EmptyMemory => "container.memory must not be empty".to_string(),
        InvalidContainerConfig::ZeroPidsLimit => "container.pidsLimit must be greater than zero".to_string(),
        InvalidContainerConfig::EmptyNetwork => "container.network must not be empty".to_string(),
        InvalidContainerConfig::SharedNetwork(network) => format!(
            "container.network {network:?} would share a network namespace outside the sandbox; use bridge, none or a network of your own"
        ),
    };
    anyhow::anyhow!(message)
}

/// A setting of the store of long tool results that it refuses, said with its key in
/// config.toml, where 0 is the threshold that stores nothing.
fn tool_output_error(error: &InvalidOutputStoreConfig) -> anyhow::Error {
    let message = match error {
        InvalidOutputStoreConfig::EmptyDirectory => "toolOutput.directory must not be empty".to_string(),
        InvalidOutputStoreConfig::MaxFileBytes(_) => {
            format!("toolOutput.maxFileBytes must be 1..={MAX_FILE_BYTES}")
        }
        InvalidOutputStoreConfig::ThresholdBytes { max_file_bytes, .. } => format!(
            "toolOutput.thresholdBytes must be 0, which keeps results inline, or {MIN_THRESHOLD_BYTES}..={max_file_bytes} (toolOutput.maxFileBytes)"
        ),
        // Whole days from config.toml are never too few; a host that sets the age itself is told.
        InvalidOutputStoreConfig::MaxAge(_) => {
            "toolOutput.maxAgeDays must be 0, which keeps the results, or at least 1".to_string()
        }
    };
    anyhow::anyhow!(message)
}

impl RawConfig {
    /// The configuration this file describes; `parent` is its directory.
    fn into_config(self, path: &Path, parent: &Path) -> Result<Config> {
        let Self {
            model,
            reasoning,
            max_tokens,
            tool_timeout_secs,
            approval_timeout_secs,
            secret_files: secrets,
            provider,
            execution,
            container,
            tool_output,
            models,
            mcp,
            proxy,
        } = self;
        let mut config = Config {
            path: Some(path.to_path_buf()),
            model,
            reasoning: parsed("reasoning", reasoning)?,
            max_tokens,
            tool_timeout_secs,
            approval_timeout_secs,
            secret_files: secret_files(secrets, parent)?,
            provider: ProviderSettings::from_table(provider)?,
            execution: execution.unwrap_or_else(|| LOCAL.to_string()),
            container,
            tool_output: tool_output.into_config(parent)?,
            models: HashMap::new(),
            mcp,
            proxy: proxy.map_or(Proxy::FromEnvironment, |RawProxy { http, https, no_proxy }| {
                Proxy::Through(Proxies { http, https, no_proxy })
            }),
            // Set by the caller, which expanded the file.
            credentials: Credentials::default(),
        };
        for model in models {
            ensure!(
                !config.models.contains_key(&model.id),
                "more than one [[models]] entry has the id {:?}",
                model.id
            );
            let input = model.input.unwrap_or_else(|| vec![ModelInput::Text]);
            ensure!(
                input.contains(&ModelInput::Text),
                "models.{}: input must include \"text\"",
                model.id
            );
            config.models.insert(
                model.id,
                ModelMeta {
                    context_window: model.context_window,
                    cost: model.cost,
                    takes_images: input.contains(&ModelInput::Image),
                },
            );
        }
        Ok(config)
    }
}

/// What each string of config.toml holds: the API key is kept for the assembly, which expands
/// it with the provider's own variable as the default; the MCP servers' tokens are credentials,
/// and so are the headers that name variables and the proxies that carry a user or password.
fn holds(keys: &[&str]) -> Holds {
    match keys {
        ["provider", "apiKey"] => Holds::Kept,
        ["mcp", "token"] => Holds::Credential,
        ["provider" | "mcp", "headers", _] => Holds::Header,
        ["proxy", "http" | "https"] => Holds::Address,
        _ => Holds::Text,
    }
}

/// Check what is known about the model `id`: an ID, and what the agent refuses too
/// (`Agent::new`), said here with the keys of config.toml: a context window of zero, and
/// prices that are not amounts ([`ModelCost::is_valid`]).
fn check_model(id: &str, meta: &ModelMeta) -> Result<()> {
    ensure!(!id.trim().is_empty(), "models: an entry has no id");
    ensure!(
        meta.context_window != Some(0),
        "models.{id}: contextWindow must be greater than zero"
    );
    ensure!(
        meta.cost.is_none_or(|cost| cost.is_valid()),
        "models.{id}: prices must be zero or more, per million tokens"
    );
    Ok(())
}

/// The secret files, relative to `parent`, the directory of config.toml.
fn secret_files(paths: Vec<PathBuf>, parent: &Path) -> Result<Vec<PathBuf>> {
    ensure!(
        paths.iter().all(|path| !path.as_os_str().is_empty()),
        "secretFiles: an entry must not be empty"
    );
    Ok(paths.into_iter().map(|path| parent.join(path)).collect())
}

impl RawToolOutput {
    /// The settings over the defaults: [`DEFAULT_TOOL_OUTPUT_DIRECTORY`] in each session's
    /// working directory, or an explicit directory, relative to `parent`; a zero threshold
    /// stores nothing, and a zero age keeps the results.
    fn into_config(self, parent: &Path) -> Result<OutputStoreConfig> {
        let Self {
            directory,
            threshold_bytes,
            max_file_bytes,
            max_age_days,
        } = self;
        let mut config = OutputStoreConfig::new(DEFAULT_TOOL_OUTPUT_DIRECTORY);
        if let Some(path) = directory {
            // Checked here: joined to `parent`, an empty path would name `parent` itself.
            ensure!(!path.as_os_str().is_empty(), "toolOutput.directory must not be empty");
            config.directory = parent.join(path);
        }
        if let Some(bytes) = threshold_bytes {
            config.threshold_bytes = (bytes > 0).then_some(bytes);
        }
        if let Some(bytes) = max_file_bytes {
            config.max_file_bytes = bytes;
        }
        if let Some(days) = max_age_days {
            config.max_age = (days > 0).then(|| Duration::from_secs(u64::from(days) * SECONDS_PER_DAY));
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests;
