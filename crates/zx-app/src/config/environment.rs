//! `config.toml`: where and with what an agent runs. The file is optional, and every key in
//! it is: what it leaves out keeps the built-in default. What a host and the environment
//! variables say over it is laid on by the assembly (`crate::assembly`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use zx_agent::ai::{ModelCost, ModelMeta, Reasoning};
use zx_agent::builtin::executor::{Access, ContainerConfig, InvalidContainerConfig};
use zx_agent::builtin::mcp::ServerConfig;
use zx_agent::builtin::output_store::{
    InvalidOutputStoreConfig, MAX_FILE_BYTES, MIN_THRESHOLD_BYTES, OutputStoreConfig,
};

use super::placeholders::{Credentials, Holds, expand_table};
use super::provider::ProviderSettings;
use super::{parsed, read_text};

/// Where long tool results are stored unless `toolOutput.directory` says otherwise: relative
/// to the working directory of each session.
const DEFAULT_TOOL_OUTPUT_DIRECTORY: &str = ".zx-agent/outputs";

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
    /// The token sent as `Authorization: Bearer`; unset sends none. A credential: the
    /// variables it names are hidden from commands, and its value is redacted from tool
    /// output.
    pub token: Option<String>,
    /// Headers sent with every request, by name. A value that names a variable is a
    /// credential, as the token is; one written as it is, is not.
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

/// `config.toml`, checked: the environment every agent of this user runs in.
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
    /// The credentials the file holds, but for the API key, which the assembly expands: the
    /// MCP servers' tokens and the headers that name variables.
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
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct RawToolOutput {
    directory: Option<PathBuf>,
    threshold_bytes: Option<usize>,
    max_file_bytes: Option<usize>,
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

    /// The names the model calls the configured MCP tools by, checking the `[[mcp]]` entries:
    /// their `name`, `url`, headers and `tools` as [`ServerConfig::validate`] does, whose
    /// errors name the server and those fields, and the keys that only config.toml has.
    pub(crate) fn mcp_tool_names(&self) -> Result<Vec<String>> {
        let mut servers = HashSet::new();
        let mut names = Vec::new();
        for server in &self.mcp {
            let name = server.name.as_str();
            let config = ServerConfig {
                token: server.token.clone(),
                headers: server.headers.clone().into_iter().collect(),
                ..ServerConfig::new(name, server.url.as_str(), server.tools.clone())
            };
            names.extend(config.validate().map_err(anyhow::Error::msg)?);
            ensure!(servers.insert(name), "more than one MCP server is named {name:?}");
            ensure!(
                server.timeout_secs != Some(0),
                "MCP server {name}: timeoutSecs must be greater than zero"
            );
        }
        Ok(names)
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
        Ok(())
    }
}

impl ContainerSettings {
    /// The settings of the containers commands run in: these over the executor's defaults,
    /// checked. Errors name the keys of config.toml, as for a missing `container.image`.
    pub(crate) fn container_config(&self) -> Result<ContainerConfig> {
        let Some(image) = &self.image else {
            bail!("execution = \"container\" needs container.image");
        };
        let mut config = ContainerConfig::new(image.as_str());
        if let Some(user) = &self.user {
            config.user.clone_from(user);
        }
        if let Some(memory) = &self.memory {
            config.memory.clone_from(memory);
        }
        if let Some(limit) = self.pids_limit {
            config.pids_limit = limit;
        }
        if let Some(network) = &self.network {
            config.network = network.parse().map_err(container_error)?;
        }
        if let Some(access) = self.workspace {
            config.workspace_access = access;
        }
        if let Some(access) = self.root {
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
    };
    anyhow::anyhow!(message)
}

impl RawConfig {
    /// The configuration this file describes; `parent` is its directory.
    fn into_config(self, path: &Path, parent: &Path) -> Result<Config> {
        let mut config = Config {
            path: Some(path.to_path_buf()),
            model: self.model,
            reasoning: parsed("reasoning", self.reasoning)?,
            max_tokens: self.max_tokens,
            tool_timeout_secs: self.tool_timeout_secs,
            approval_timeout_secs: self.approval_timeout_secs,
            secret_files: secret_files(self.secret_files, parent)?,
            provider: ProviderSettings::from_table(self.provider)?,
            execution: self.execution.unwrap_or_else(|| LOCAL.to_string()),
            container: self.container,
            tool_output: self.tool_output.into_config(parent)?,
            models: HashMap::new(),
            mcp: self.mcp,
            // Set by the caller, which expanded the file.
            credentials: Credentials::default(),
        };
        for model in self.models {
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
/// and so are the headers that name variables.
fn holds(keys: &[&str]) -> Holds {
    match keys {
        ["provider", "apiKey"] => Holds::Kept,
        ["mcp", "token"] => Holds::Credential,
        ["provider" | "mcp", "headers", _] => Holds::Header,
        _ => Holds::Text,
    }
}

/// Check what is known about the model `id`: an ID, a positive context window, and prices
/// that are numbers, zero or more.
fn check_model(id: &str, meta: &ModelMeta) -> Result<()> {
    ensure!(!id.trim().is_empty(), "models: an entry has no id");
    ensure!(
        meta.context_window.is_none_or(|window| window > 0),
        "models.{id}: contextWindow must be greater than zero"
    );
    if let Some(cost) = &meta.cost {
        ensure!(
            [cost.input, cost.output, cost.cache_read, cost.cache_write]
                .iter()
                .all(|price| price.is_finite() && *price >= 0.0),
            "models.{id}: prices must be zero or more, per million tokens"
        );
    }
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
    /// stores nothing.
    fn into_config(self, parent: &Path) -> Result<OutputStoreConfig> {
        let mut config = OutputStoreConfig::new(DEFAULT_TOOL_OUTPUT_DIRECTORY);
        if let Some(path) = self.directory {
            // Checked here: joined to `parent`, an empty path would name `parent` itself.
            ensure!(!path.as_os_str().is_empty(), "toolOutput.directory must not be empty");
            config.directory = parent.join(path);
        }
        if let Some(bytes) = self.threshold_bytes {
            config.threshold_bytes = (bytes > 0).then_some(bytes);
        }
        if let Some(bytes) = self.max_file_bytes {
            config.max_file_bytes = bytes;
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests;
