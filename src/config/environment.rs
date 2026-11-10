//! `config.toml`: where and with what an agent runs. The file is optional, and every key in
//! it is: what it leaves out keeps the built-in default. The command line and its variables
//! go over it; that is up to the frontend.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

use super::read_text;
use crate::ai::openai::{ReasoningFormat, SystemUpdates};
use crate::ai::{ModelCost, ModelMeta, Reasoning};
use crate::executor::{Access, check_network};
use crate::mcp::{check_server_name, tool_name as mcp_tool_name};
use crate::output_store;

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

/// An external MCP server whose listed tools the model may call. The frontends connect it at
/// startup. Its tools ask a person, and are refused where no one can answer, as in `run`,
/// unless the agent's `permissions.allow` lists them.
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
    /// unset uses `--tool-timeout-secs`.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Bound for a whole call, in seconds, however much progress the server reports; unset
    /// leaves only `timeoutSecs`, the run's timeout and cancellation.
    #[serde(default)]
    pub max_timeout_secs: Option<u64>,
}

/// The `[provider]` table: how the model is reached. Unset values leave the choice to the
/// command line or its default.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderSettings {
    /// OpenAI-compatible base URL, without `/chat/completions`.
    pub base_url: Option<String>,
    /// Name of the variable holding the API key.
    pub api_key_env: Option<String>,
    /// How system prompt changes reach the model.
    pub system_updates: Option<SystemUpdates>,
    /// How a reasoning level is written into the request.
    pub reasoning_format: Option<ReasoningFormat>,
    /// Send an empty `reasoning_content` with answers that lack one.
    pub fill_missing_reasoning: bool,
    /// Send the session ID as the request's `prompt_cache_key`.
    pub prompt_cache_key: bool,
}

/// `config.toml`, checked: the environment every agent of this user runs in.
#[derive(Debug, Default)]
pub struct Config {
    /// Where the file is, or would be; `None` without a home directory.
    pub path: Option<PathBuf>,
    /// The model ID, unless the command line names one.
    pub model: Option<String>,
    /// How hard a reasoning model thinks, unless the command line says.
    pub reasoning: Option<Reasoning>,
    /// How the model is reached.
    pub provider: ProviderSettings,
    /// Where bash and read_file run.
    pub execution: Execution,
    /// Used when `execution` is `Container`.
    pub container: ContainerSettings,
    /// Where long tool results are saved when read_output is enabled.
    pub tool_output: output_store::Config,
    /// What is known about models, by ID (`[[models]]`): the context window and prices.
    pub models: HashMap<String, ModelMeta>,
    /// External MCP servers.
    pub mcp: Vec<McpServer>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct RawConfig {
    model: Option<String>,
    reasoning: Option<String>,
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
    fill_missing_reasoning: bool,
    prompt_cache_key: bool,
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

    /// The names the model calls the configured MCP tools by, checking the `[[mcp]]` entries.
    pub fn mcp_tool_names(&self) -> Result<Vec<String>> {
        let mut servers = HashSet::new();
        let mut names = Vec::new();
        for server in &self.mcp {
            let name = server.name.as_str();
            check_server_name(name).map_err(anyhow::Error::msg)?;
            ensure!(servers.insert(name), "more than one MCP server is named {name:?}");
            ensure!(!server.url.trim().is_empty(), "MCP server {name}: url is empty");
            ensure!(!server.tools.is_empty(), "MCP server {name}: tools lists no tool");
            ensure!(
                server.token_env.as_deref().is_none_or(|env| !env.trim().is_empty()),
                "MCP server {name}: tokenEnv is empty"
            );
            ensure!(
                server.timeout_secs != Some(0),
                "MCP server {name}: timeoutSecs must be greater than zero"
            );
            ensure!(
                server.max_timeout_secs != Some(0),
                "MCP server {name}: maxTimeoutSecs must be greater than zero"
            );
            let first = names.len();
            for tool in &server.tools {
                let model_name = mcp_tool_name(name, tool).map_err(anyhow::Error::msg)?;
                ensure!(
                    !names[first..].contains(&model_name),
                    "MCP server {name}: tool {tool:?} would be named {model_name:?}, like another of its tools"
                );
                names.push(model_name);
            }
        }
        Ok(names)
    }

    /// Check the container settings, the tool output settings and the MCP servers.
    fn validate(&self) -> Result<()> {
        self.tool_output.validate().context("[toolOutput]")?;
        for (key, value) in [
            ("container.image", &self.container.image),
            ("container.user", &self.container.user),
            ("container.memory", &self.container.memory),
            ("container.network", &self.container.network),
        ] {
            ensure!(
                value.as_deref().is_none_or(|value| !value.trim().is_empty()),
                "{key} must not be empty"
            );
        }
        ensure!(
            self.container.pids_limit != Some(0),
            "container.pidsLimit must be greater than zero"
        );
        if let Some(network) = &self.container.network {
            check_network(network).context("container.network")?;
        }
        ensure!(
            self.execution == Execution::Local || self.container.image.is_some(),
            "execution = \"container\" needs container.image"
        );
        self.mcp_tool_names()?;
        Ok(())
    }
}

impl RawConfig {
    /// The configuration this file describes; `parent` is its directory.
    fn into_config(self, path: &Path, parent: &Path) -> Result<Config> {
        let mut config = Config {
            path: Some(path.to_path_buf()),
            model: non_empty("model", self.model)?,
            reasoning: parsed("reasoning", self.reasoning)?,
            provider: self.provider.into_settings()?,
            execution: self.execution.unwrap_or_default(),
            container: self.container,
            tool_output: self.tool_output.into_config(parent)?,
            models: HashMap::new(),
            mcp: self.mcp,
        };
        for model in self.models {
            model.add_to(&mut config.models)?;
        }
        Ok(config)
    }
}

impl RawProvider {
    fn into_settings(self) -> Result<ProviderSettings> {
        Ok(ProviderSettings {
            base_url: non_empty("provider.baseUrl", self.base_url)?,
            api_key_env: non_empty("provider.apiKeyEnv", self.api_key_env)?,
            system_updates: parsed("provider.systemUpdates", self.system_updates)?,
            reasoning_format: parsed("provider.reasoningFormat", self.reasoning_format)?,
            fill_missing_reasoning: self.fill_missing_reasoning,
            prompt_cache_key: self.prompt_cache_key,
        })
    }
}

/// `value`, which must not be empty when it is set.
fn non_empty(key: &str, value: Option<String>) -> Result<Option<String>> {
    ensure!(
        value.as_deref().is_none_or(|value| !value.trim().is_empty()),
        "{key} must not be empty"
    );
    Ok(value)
}

/// `value` parsed as the command line parses the option of the same name.
fn parsed<T: std::str::FromStr<Err = String>>(key: &str, value: Option<String>) -> Result<Option<T>> {
    value
        .map(|text| text.parse().map_err(|error: String| anyhow::anyhow!("{key}: {error}")))
        .transpose()
}

impl RawToolOutput {
    /// The settings over the defaults; an explicit directory is relative to `parent`.
    fn into_config(self, parent: &Path) -> Result<output_store::Config> {
        let mut config = output_store::Config::default();
        if let Some(path) = self.directory {
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

impl RawModel {
    /// Check the entry and record it under its id.
    fn add_to(self, models: &mut HashMap<String, ModelMeta>) -> Result<()> {
        ensure!(!self.id.trim().is_empty(), "models: an entry has no id");
        ensure!(
            self.context_window.is_none_or(|window| window > 0),
            "models.{}: contextWindow must be greater than zero",
            self.id
        );
        if let Some(cost) = &self.cost {
            ensure!(
                [cost.input, cost.output, cost.cache_read, cost.cache_write]
                    .iter()
                    .all(|price| price.is_finite() && *price >= 0.0),
                "models.{}: prices must be zero or more, per million tokens",
                self.id
            );
        }
        models.insert(
            self.id,
            ModelMeta {
                context_window: self.context_window,
                cost: self.cost,
            },
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
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
        assert_eq!(config.provider, ProviderSettings::default());
        assert_eq!(config.execution, Execution::Local);
        assert_eq!(config.tool_output.threshold_bytes, Some(8 * 1024));
        assert!(config.mcp.is_empty() && config.models.is_empty());
    }

    #[test]
    fn the_model_and_its_provider_are_read_with_the_command_line_spellings() {
        let config = load_text(
            "model = 'm1'\nreasoning = 'high'\n[provider]\nbaseUrl = 'http://localhost:8000/v1'\n\
             apiKeyEnv = 'LOCAL_KEY'\nsystemUpdates = 'inline'\nreasoningFormat = 'thinking'\n\
             fillMissingReasoning = true\npromptCacheKey = true",
        )
        .unwrap();
        assert_eq!(config.model.as_deref(), Some("m1"));
        assert_eq!(config.reasoning, Some(Reasoning::High));
        assert_eq!(
            config.provider,
            ProviderSettings {
                base_url: Some("http://localhost:8000/v1".into()),
                api_key_env: Some("LOCAL_KEY".into()),
                system_updates: Some(SystemUpdates::Inline),
                reasoning_format: Some(ReasoningFormat::Thinking),
                fill_missing_reasoning: true,
                prompt_cache_key: true,
            }
        );
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
        ] {
            let error = error(text);
            assert!(error.contains(expected), "{text}: {error}");
        }
    }

    const PROM: &str = "[[mcp]]\nname = 'prom'\nurl = 'https://prom.example/mcp'\ntokenEnv = 'PROM_TOKEN'\n\
                        tools = ['query', 'range']\ntimeoutSecs = 30\n\
                        maxTimeoutSecs = 600\n";

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
                max_timeout_secs: Some(600),
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
                server("tools = ['query']\nmaxTimeoutSecs = 0"),
                "maxTimeoutSecs must be greater than zero",
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
