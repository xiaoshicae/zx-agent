//! `config.toml`: where and with what an agent runs. The file is optional, and every key in
//! it is: what it leaves out keeps the built-in default. What a host and the environment
//! variables say over it is laid on by the assembly (`crate::assembly`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

use super::read_text;
use zx_agent::ai::openai::OpenAiConfig;
use zx_agent::ai::retry::RetryPolicy;
use zx_agent::ai::{ModelCost, ModelMeta, Reasoning};
use zx_agent::builtin::executor::{Access, ContainerConfig, InvalidContainerConfig};
use zx_agent::builtin::mcp::ServerConfig;
use zx_agent::builtin::output_store::{
    InvalidOutputStoreConfig, MAX_FILE_BYTES, MIN_THRESHOLD_BYTES, OutputStoreConfig,
};

/// The variable holding the API key when config.toml names none.
const DEFAULT_API_KEY_ENV: &str = "OPENAI_API_KEY";

/// Where long tool results are stored unless `toolOutput.directory` says otherwise: relative
/// to the working directory of each session.
const DEFAULT_TOOL_OUTPUT_DIRECTORY: &str = ".zx-agent/outputs";

/// Where bash and read_file run.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Execution {
    /// On this machine, as the agent's user.
    #[default]
    Local,
    /// In an isolated Docker container; see [`ContainerSettings`].
    Container,
}

/// Container settings for `Execution::Container`, the `[container]` table; unset values use
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
/// agent's `permissions.allow` lists them.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct McpServer {
    /// ASCII lowercase letters, digits and `-`. Its tools reach the model as `<name>__<tool>`.
    pub name: String,
    /// The server's MCP endpoint (Streamable HTTP).
    pub url: String,
    /// The variable holding the token sent as `Authorization: Bearer`; unset sends none.
    /// Commands do not see the variable, and its value is redacted from tool output.
    #[serde(default)]
    pub token_env: Option<String>,
    /// The server's tools to offer the model, by the names the server gives them.
    pub tools: Vec<String>,
    /// Longest a call may go without an answer or a progress notification, in seconds;
    /// unset uses `toolTimeoutSecs`. A call that keeps reporting progress is bounded by the
    /// run's timeout and cancellation.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// The `[provider]` table over the built-in defaults, the provider's own: how the model is
/// reached.
#[derive(Clone, Debug)]
pub struct ProviderSettings {
    /// Where requests go and how they are written; the API key is not here: the assembly
    /// reads it from the variable `api_key_env` names.
    pub openai: OpenAiConfig,
    /// Name of the variable holding the API key.
    pub api_key_env: String,
    /// How transient provider errors are retried; the file sets only the count.
    pub retry: RetryPolicy,
}

impl Default for ProviderSettings {
    fn default() -> Self {
        Self {
            openai: OpenAiConfig::default(),
            api_key_env: DEFAULT_API_KEY_ENV.to_string(),
            retry: RetryPolicy::default(),
        }
    }
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
    /// Where bash and read_file run.
    pub execution: Execution,
    /// Used when `execution` is `Container`.
    pub container: ContainerSettings,
    /// Where long tool results are saved when read_output is enabled.
    pub tool_output: OutputStoreConfig,
    /// What is known about models, by ID (`[[models]]`): the context window and prices.
    pub models: HashMap<String, ModelMeta>,
    /// External MCP servers.
    pub mcp: Vec<McpServer>,
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
            execution: Execution::default(),
            container: ContainerSettings::default(),
            tool_output: OutputStoreConfig::new(DEFAULT_TOOL_OUTPUT_DIRECTORY),
            models: HashMap::new(),
            mcp: Vec::new(),
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
    provider: RawProvider,
    // Unknown backends fail to deserialize instead of being ignored.
    execution: Option<Execution>,
    container: ContainerSettings,
    tool_output: RawToolOutput,
    models: Vec<RawModel>,
    mcp: Vec<McpServer>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct RawProvider {
    base_url: Option<String>,
    api_key_env: Option<String>,
    system_updates: Option<String>,
    reasoning_format: Option<String>,
    max_tokens_field: Option<String>,
    fill_missing_reasoning: bool,
    prompt_cache_key: bool,
    request_fields: serde_json::Map<String, serde_json::Value>,
    headers: BTreeMap<String, String>,
    idle_timeout_secs: Option<u64>,
    max_retries: Option<u32>,
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
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default)]
    cost: Option<ModelCost>,
}

impl Config {
    /// Read and check the file at `path`; a file that does not exist is an empty one. Paths in
    /// the file are relative to its directory.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = match std::fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => RawConfig::default(),
            _ => toml::from_str(&read_text(path)?).with_context(|| format!("invalid {}", path.display()))?,
        };
        // A file path always has a parent; the root stands in for one that has none.
        let parent = path.parent().unwrap_or_else(|| Path::new("/"));
        let config = raw
            .into_config(path, parent)
            .with_context(|| format!("invalid {}", path.display()))?;
        config
            .validate()
            .with_context(|| format!("invalid {}", path.display()))?;
        Ok(config)
    }

    /// The names the model calls the configured MCP tools by, checking the `[[mcp]]` entries:
    /// their `name`, `url` and `tools` as [`ServerConfig::validate`] does, whose errors name
    /// the server and those fields, and the keys that only config.toml has.
    pub(crate) fn mcp_tool_names(&self) -> Result<Vec<String>> {
        let mut servers = HashSet::new();
        let mut names = Vec::new();
        for server in &self.mcp {
            let config = ServerConfig::new(server.name.as_str(), server.url.as_str(), server.tools.clone());
            names.extend(config.validate().map_err(anyhow::Error::msg)?);
            let name = server.name.as_str();
            ensure!(servers.insert(name), "more than one MCP server is named {name:?}");
            ensure!(
                server.token_env.as_deref().is_none_or(|env| !env.trim().is_empty()),
                "MCP server {name}: tokenEnv is empty"
            );
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
            ("toolTimeoutSecs", self.tool_timeout_secs),
            ("approvalTimeoutSecs", self.approval_timeout_secs),
        ] {
            ensure!(value != Some(0), "{key} must be at least 1");
        }
        self.provider.validate()?;
        for (id, meta) in &self.models {
            check_model(id, meta)?;
        }
        self.tool_output.validate().map_err(|error| tool_output_error(&error))?;
        // Without an image no container starts, and the rest of [container] is not used.
        if self.execution == Execution::Container || self.container.image.is_some() {
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
            provider: self.provider.into_settings()?,
            execution: self.execution.unwrap_or_default(),
            container: self.container,
            tool_output: self.tool_output.into_config(parent)?,
            models: HashMap::new(),
            mcp: self.mcp,
        };
        for model in self.models {
            ensure!(
                !config.models.contains_key(&model.id),
                "more than one [[models]] entry has the id {:?}",
                model.id
            );
            config.models.insert(
                model.id,
                ModelMeta {
                    context_window: model.context_window,
                    cost: model.cost,
                },
            );
        }
        Ok(config)
    }
}

impl RawProvider {
    /// The settings over the defaults, checked.
    fn into_settings(self) -> Result<ProviderSettings> {
        let mut settings = ProviderSettings::default();
        let ProviderSettings {
            openai,
            api_key_env,
            retry,
        } = &mut settings;
        if let Some(url) = self.base_url {
            openai.base_url = url;
        }
        if let Some(env) = self.api_key_env {
            *api_key_env = env;
        }
        if let Some(updates) = parsed("provider.systemUpdates", self.system_updates)? {
            openai.body.system_updates = updates;
        }
        if let Some(format) = parsed("provider.reasoningFormat", self.reasoning_format)? {
            openai.body.reasoning_format = format;
        }
        if let Some(field) = parsed("provider.maxTokensField", self.max_tokens_field)? {
            openai.body.max_tokens_field = field;
        }
        openai.body.fill_missing_reasoning = self.fill_missing_reasoning;
        openai.body.prompt_cache_key = self.prompt_cache_key;
        openai.body.extra_fields = self.request_fields;
        openai.headers = self.headers.into_iter().collect();
        if let Some(secs) = self.idle_timeout_secs {
            openai.idle_timeout = Duration::from_secs(secs);
        }
        if let Some(retries) = self.max_retries {
            retry.max_retries = retries;
        }
        Ok(settings)
    }
}

impl ProviderSettings {
    /// Check the provider's values: an address and a key variable that are not empty, a
    /// positive idle timeout, request fields and headers with names, and no `Authorization`
    /// header, which comes from the API key.
    fn validate(&self) -> Result<()> {
        let openai = &self.openai;
        ensure!(!openai.base_url.trim().is_empty(), "provider.baseUrl must not be empty");
        ensure!(
            !self.api_key_env.trim().is_empty(),
            "provider.apiKeyEnv must not be empty"
        );
        ensure!(
            !openai.idle_timeout.is_zero(),
            "provider.idleTimeoutSecs must be at least 1"
        );
        ensure!(
            openai.body.extra_fields.keys().all(|field| !field.trim().is_empty()),
            "provider.requestFields: a field has no name"
        );
        for (name, _) in &openai.headers {
            ensure!(!name.trim().is_empty(), "provider.headers: a header has no name");
            ensure!(
                !name.trim().eq_ignore_ascii_case("authorization"),
                "provider.headers: Authorization comes from the API key (provider.apiKeyEnv); it cannot be set as a header"
            );
        }
        Ok(())
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
        "secretFiles: an entry is empty"
    );
    Ok(paths.into_iter().map(|path| parent.join(path)).collect())
}

/// `value` parsed by its type, with `key` naming it in the error.
fn parsed<T: std::str::FromStr<Err = String>>(key: &str, value: Option<String>) -> Result<Option<T>> {
    value
        .map(|text| text.parse().map_err(|error: String| anyhow::anyhow!("{key}: {error}")))
        .transpose()
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
mod tests {
    use zx_agent::ai::openai::{MaxTokensField, ReasoningFormat, SystemUpdates};

    use super::*;

    fn load_text(text: &str) -> Result<Config> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        Config::load(&path)
    }

    fn error(text: &str) -> String {
        format!("{:#}", load_text(text).unwrap_err())
    }

    #[test]
    fn a_missing_file_is_an_empty_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = Config::load(&path).unwrap();
        assert_eq!(config.path.as_deref(), Some(path.as_path()));
        assert_eq!(config.model, None);
        assert_eq!(config.provider.openai.base_url, "https://api.openai.com/v1");
        assert_eq!(config.provider.api_key_env, "OPENAI_API_KEY");
        assert_eq!(config.provider.retry.max_retries, 3);
        assert_eq!(config.execution, Execution::Local);
        assert_eq!(config.tool_output.threshold_bytes, Some(8 * 1024));
        // In each session's working directory.
        assert_eq!(config.tool_output.directory, Path::new(".zx-agent/outputs"));
        assert!(config.mcp.is_empty() && config.models.is_empty());
    }

    #[test]
    fn the_model_and_its_provider_are_read_with_the_spellings_of_the_former_options() {
        let config = load_text(
            "model = 'm1'\nreasoning = 'high'\nmaxTokens = 512\ntoolTimeoutSecs = 30\napprovalTimeoutSecs = 60\n\
             [provider]\nbaseUrl = 'http://localhost:8000/v1'\n\
             apiKeyEnv = 'LOCAL_KEY'\nsystemUpdates = 'inline'\nreasoningFormat = 'thinking'\n\
             maxTokensField = 'max_completion_tokens'\n\
             fillMissingReasoning = true\npromptCacheKey = true\nidleTimeoutSecs = 15\nmaxRetries = 0\n\
             requestFields = { thinking = { type = 'disabled' }, top_k = 5 }\n\
             headers = { X-Route = 'eu' }",
        )
        .unwrap();
        assert_eq!(config.model.as_deref(), Some("m1"));
        assert_eq!(config.reasoning, Some(Reasoning::High));
        assert_eq!((config.max_tokens, config.tool_timeout_secs), (Some(512), Some(30)));
        let mut fields = serde_json::Map::new();
        fields.insert("thinking".into(), serde_json::json!({"type": "disabled"}));
        fields.insert("top_k".into(), serde_json::json!(5));
        let provider = &config.provider;
        let openai = &provider.openai;
        assert_eq!(openai.base_url, "http://localhost:8000/v1");
        assert_eq!(provider.api_key_env, "LOCAL_KEY");
        assert_eq!(openai.body.system_updates, SystemUpdates::Inline);
        assert_eq!(openai.body.reasoning_format, ReasoningFormat::Thinking);
        assert_eq!(openai.body.max_tokens_field, MaxTokensField::MaxCompletionTokens);
        assert!(openai.body.fill_missing_reasoning && openai.body.prompt_cache_key);
        assert_eq!(openai.body.extra_fields, fields);
        assert_eq!(openai.headers, [("X-Route".to_string(), "eu".to_string())]);
        assert_eq!(openai.idle_timeout, Duration::from_secs(15));
        assert_eq!(provider.retry.max_retries, 0);
        assert_eq!(config.approval_timeout_secs, Some(60));
    }

    #[test]
    fn secret_files_are_relative_to_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "secretFiles = ['token', '/run/secrets/other']").unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(
            config.secret_files,
            [dir.path().join("token"), PathBuf::from("/run/secrets/other")]
        );
    }

    #[test]
    fn invalid_values_of_the_former_options_name_their_key() {
        for (text, expected) in [
            ("toolTimeoutSecs = 0", "toolTimeoutSecs must be at least 1"),
            (
                "[provider]\nidleTimeoutSecs = 0",
                "provider.idleTimeoutSecs must be at least 1",
            ),
            ("approvalTimeoutSecs = 0", "approvalTimeoutSecs must be at least 1"),
            ("[chat]\nqueueMode = 'all'", "unknown field `chat`"),
            ("[chat]\napprovalTimeoutSecs = 60", "unknown field `chat`"),
            ("[acp]\napprovalTimeoutSecs = 60", "unknown field `acp`"),
            (
                "[provider]\nheaders = { Authorization = 'Bearer x' }",
                "provider.headers: Authorization",
            ),
            (
                "[provider]\nheaders = { ' ' = 'x' }",
                "provider.headers: a header has no name",
            ),
            (
                "[provider]\nrequestFields = { ' ' = 1 }",
                "provider.requestFields: a field has no name",
            ),
            ("[provider]\nrequestFields = { seed = 1, seed = 2 }", "duplicate key"),
            ("[provider]\nrequestFields = 'seed=1'", "invalid type"),
            ("[provider]\nmaxRetries = -1", "maxRetries"),
            ("secretFiles = ['']", "secretFiles: an entry is empty"),
            ("maxTokens = 'many'", "maxTokens"),
        ] {
            let error = error(text);
            assert!(error.contains(expected), "{text}: {error}");
        }
    }

    #[test]
    fn unknown_keys_and_invalid_values_are_errors_that_name_the_file() {
        for (text, expected) in [
            ("typo = true", "unknown field `typo`"),
            ("[provider]\napiKey = 'sk'", "unknown field `apiKey`"),
            ("model = ' '", "model must not be empty"),
            ("reasoning = 'hard'", "unknown reasoning level"),
            ("[provider]\nsystemUpdates = 'merge'", "provider.systemUpdates"),
            ("[provider]\nreasoningFormat = 'x'", "provider.reasoningFormat"),
            (
                "[provider]\nmaxTokensField = 'max_output_tokens'",
                "provider.maxTokensField",
            ),
            ("[provider]\nbaseUrl = ''", "provider.baseUrl must not be empty"),
            ("execution = 'kubernetes'", "unknown variant"),
        ] {
            let error = error(text);
            assert!(error.contains(expected), "{text}: {error}");
            assert!(error.contains("config.toml"), "{text}: {error}");
        }
    }

    #[test]
    fn container_execution_reads_its_settings_and_defaults_to_local() {
        let config = load_text(
            "execution = 'container'\n[container]\nimage = 'debian:bookworm-slim'\nuser = '1000:1000'\nmemory = '512m'\n\
             pidsLimit = 64\nnetwork = 'none'\nworkspace = 'readOnly'\nroot = 'readOnly'",
        )
        .unwrap();
        assert_eq!(config.execution, Execution::Container);
        assert_eq!(
            config.container,
            ContainerSettings {
                image: Some("debian:bookworm-slim".into()),
                user: Some("1000:1000".into()),
                memory: Some("512m".into()),
                pids_limit: Some(64),
                network: Some("none".into()),
                workspace: Some(Access::ReadOnly),
                root: Some(Access::ReadOnly),
            }
        );
        // Unset, the sandbox settings are left to the executor, whose defaults are open.
        let open = load_text("execution = 'container'\n[container]\nimage = 'x'").unwrap();
        assert_eq!(
            (open.container.network, open.container.workspace, open.container.root),
            (None, None, None)
        );
    }

    #[test]
    fn invalid_container_settings_are_configuration_errors() {
        for text in [
            "execution = 'container'",
            "execution = 'container'\n[container]\nimage = ' '",
            "[container]\nimage = 'x'\npidsLimit = 0",
            "[container]\nimage = 'x'\nuser = ''",
            "[container]\nimage = 'x'\nnetwork = 'host'",
            "[container]\nimage = 'x'\nnetwork = 'container:other'",
            "[container]\nimage = 'x'\nnetwork = ' '",
            "[container]\nimage = 'x'\nworkspace = 'ro'",
            "[container]\nimage = 'x'\nroot = 'writable'",
        ] {
            assert!(load_text(text).is_err(), "{text}");
        }
    }

    #[test]
    fn the_executors_refusals_of_container_settings_name_their_keys() {
        for (text, expected) in [
            (
                "[container]\nimage = 'x'\nuser = 'root'",
                "container.user \"root\" is root; commands must run as an unprivileged user",
            ),
            (
                "execution = 'container'\n[container]\nimage = 'x'\nuser = '0:0'",
                "container.user \"0:0\" is root",
            ),
            (
                "[container]\nimage = 'x'\npidsLimit = 0",
                "container.pidsLimit must be greater than zero",
            ),
            (
                "[container]\nimage = 'x'\nnetwork = 'host'",
                "container.network \"host\" would share a network namespace outside the sandbox",
            ),
            (
                "execution = 'container'",
                "execution = \"container\" needs container.image",
            ),
        ] {
            let error = error(text);
            assert!(error.contains(expected), "{text}: {error}");
        }
    }

    #[test]
    fn an_output_directory_is_relative_to_the_file_and_settings_validate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[toolOutput]\ndirectory = 'results'\nthresholdBytes = 0").unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.tool_output.directory, dir.path().join("results"));
        assert_eq!(config.tool_output.threshold_bytes, None);
        for text in [
            "[toolOutput]\ndirectory = ''",
            "[toolOutput]\nthreshold_bytes = 256",
            "[toolOutput]\nthresholdBytes = 1",
            "[toolOutput]\nmaxFileBytes = 1048577",
        ] {
            assert!(load_text(text).is_err(), "{text}");
        }
    }

    #[test]
    fn the_stores_refusals_of_output_settings_name_their_keys() {
        for (text, expected) in [
            (
                "[toolOutput]\nthresholdBytes = 1",
                "toolOutput.thresholdBytes must be 0, which keeps results inline, or 256..=1048576 (toolOutput.maxFileBytes)",
            ),
            (
                "[toolOutput]\nmaxFileBytes = 1000\nthresholdBytes = 2000",
                "or 256..=1000 (toolOutput.maxFileBytes)",
            ),
            (
                "[toolOutput]\nmaxFileBytes = 1048577",
                "toolOutput.maxFileBytes must be 1..=1048576",
            ),
            ("[toolOutput]\ndirectory = ''", "toolOutput.directory must not be empty"),
        ] {
            let error = error(text);
            assert!(error.contains(expected), "{text}: {error}");
            assert!(!error.contains("threshold_bytes") && !error.contains("None"), "{error}");
        }
    }

    #[test]
    fn models_are_known_by_id_and_their_prices_checked() {
        let config =
            load_text("[[models]]\nid = 'm'\ncontextWindow = 1000\ncost = { input = 1.0, output = 2.0 }").unwrap();
        assert_eq!(config.models["m"].context_window, Some(1000));
        for (text, expected) in [
            ("[[models]]\nid = ' '", "an entry has no id"),
            (
                "[[models]]\nid = 'm'\ncontextWindow = 0",
                "contextWindow must be greater than zero",
            ),
            (
                "[[models]]\nid = 'm'\ncost = { input = -1.0, output = 0.0 }",
                "prices must be zero or more",
            ),
            (
                "[[models]]\nid = 'm'\ncontextWindow = 1000\n[[models]]\nid = 'm'\ncontextWindow = 2000",
                "more than one [[models]] entry has the id \"m\"",
            ),
        ] {
            let error = error(text);
            assert!(error.contains(expected), "{text}: {error}");
        }
    }

    const PROM: &str = "[[mcp]]\nname = 'prom'\nurl = 'https://prom.example/mcp'\ntokenEnv = 'PROM_TOKEN'\n\
                        tools = ['query', 'range']\ntimeoutSecs = 30\n";

    #[test]
    fn mcp_servers_are_read_from_the_file() {
        let config = load_text(PROM).unwrap();
        assert_eq!(
            config.mcp,
            [McpServer {
                name: "prom".into(),
                url: "https://prom.example/mcp".into(),
                token_env: Some("PROM_TOKEN".into()),
                tools: vec!["query".into(), "range".into()],
                timeout_secs: Some(30),
            }]
        );
        assert_eq!(config.mcp_tool_names().unwrap(), ["prom__query", "prom__range"]);
    }

    #[test]
    fn malformed_mcp_settings_are_refused() {
        let server = |fields: &str| format!("[[mcp]]\nname = 'prom'\nurl = 'https://prom.example/mcp'\n{fields}\n");
        for (text, expected) in [
            (server("tools = []"), "tools lists no tool"),
            (
                server("tools = ['query']\nallowTools = ['query']"),
                "unknown field `allowTools`",
            ),
            (
                server("tools = ['query']\ntimeoutSecs = 0"),
                "timeoutSecs must be greater than zero",
            ),
            (
                server("tools = ['query']\nmaxTimeoutSecs = 600"),
                "unknown field `maxTimeoutSecs`",
            ),
            (server("tools = ['query']\ntokenEnv = ' '"), "tokenEnv is empty"),
            (server("tools = ['a.b', 'a_b']"), "like another of its tools"),
            (server(&format!("tools = ['{}']", "x".repeat(64))), "longer than 64"),
            (
                format!("{}{}", server("tools = ['query']"), server("tools = ['range']")),
                "more than one MCP server",
            ),
            (
                "[[mcp]]\nname = 'Prom'\nurl = 'https://prom.example/mcp'\ntools = ['query']\n".to_string(),
                "must be ASCII lowercase",
            ),
            (server("tools = ['query']\nheaders = {}"), "unknown field"),
        ] {
            let error = error(&text);
            assert!(error.contains(expected), "{text}\n{error}");
        }
    }

    #[test]
    fn the_file_is_read_within_bounds() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Config::load(dir.path()).is_err());
        let path = dir.path().join("config.toml");
        std::fs::write(&path, format!("# {}", "x".repeat(64 * 1024))).unwrap();
        assert!(format!("{:#}", Config::load(&path).unwrap_err()).contains("exceeds"));
    }
}
