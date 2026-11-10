//! The environment a process runs in: config.toml, with the model of the command line and
//! two variables over it.
//!
//! The model comes from `--model`, else `ZX_MODEL` (clap reads both), else config.toml; the
//! provider's address from `ZX_BASE_URL`, else config.toml, else the provider's default
//! (`OpenAiConfig::default()`). Every other setting comes from config.toml or its built-in
//! default.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};

use zx_agent::config::{self, Config};

use super::env_value;

/// The variable that names the provider's address over config.toml.
const BASE_URL_ENV: &str = "ZX_BASE_URL";
/// Seconds a bash command may run, and an MCP call wait, unless config.toml says otherwise.
const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 120;
/// How long chat, acp and im wait for an approval by default, in seconds: long enough to
/// notice a card in an IM thread, short enough that a forgotten one does not hold a task.
const DEFAULT_APPROVAL_TIMEOUT_SECS: u64 = 600;

/// config.toml with the command line over it, the defaults filled in.
pub(crate) struct Settings {
    pub(crate) model: String,
    pub(super) tool_timeout: Duration,
    /// How long chat, acp and im wait for an approval.
    pub(crate) approval_timeout: Duration,
    /// The rest of config.toml: the provider, with `ZX_BASE_URL` over its address, the
    /// reasoning level, where commands run, the MCP servers, the models, the secret files and
    /// where long tool results go.
    pub(crate) config: Config,
    /// The zx-agent home directory, whose `agents/` holds the user's agents.
    pub(super) home: Option<PathBuf>,
}

impl Settings {
    /// Read config.toml from the zx-agent home and lay `model`, the command line's, and
    /// `ZX_BASE_URL` over it. A process without a model anywhere is a configuration error.
    pub(super) fn resolve(model: Option<&str>) -> Result<Self> {
        let home = config::home()?;
        let config = match &home {
            Some(home) => Config::load(&home.join("config.toml"))?,
            None => Config::default(),
        };
        Self::over(model, env_value(BASE_URL_ENV), config, home)
    }

    /// `model` and `base_url` over `config`, read from `home`.
    fn over(model: Option<&str>, base_url: Option<String>, mut config: Config, home: Option<PathBuf>) -> Result<Self> {
        let model = model
            .map(str::to_string)
            .or_else(|| config.model.clone())
            .with_context(|| {
                let place = config
                    .path
                    .as_ref()
                    .map_or_else(|| "config.toml".to_string(), |path| path.display().to_string());
                format!("no model: pass --model, set ZX_MODEL, or set model in {place}")
            })?;
        anyhow::ensure!(!model.trim().is_empty(), "the model is empty");
        if let Some(url) = base_url {
            config.provider.openai.base_url = url;
        }
        let seconds = |value: Option<u64>, default| Duration::from_secs(value.unwrap_or(default));
        Ok(Self {
            model,
            tool_timeout: seconds(config.tool_timeout_secs, DEFAULT_TOOL_TIMEOUT_SECS),
            approval_timeout: seconds(config.approval_timeout_secs, DEFAULT_APPROVAL_TIMEOUT_SECS),
            config,
            home,
        })
    }
}

#[cfg(test)]
mod tests {
    use zx_agent::ai::Reasoning;
    use zx_agent::ai::openai::{ReasoningFormat, SystemUpdates};

    use super::*;

    /// A config.toml that sets every key with a built-in default.
    fn file() -> Config {
        let mut config = Config {
            path: Some(PathBuf::from("/home/me/.zx-agent/config.toml")),
            model: Some("file-model".into()),
            reasoning: Some(Reasoning::Low),
            max_tokens: Some(512),
            tool_timeout_secs: Some(30),
            approval_timeout_secs: Some(60),
            ..Config::default()
        };
        let provider = &mut config.provider;
        provider.openai.base_url = "http://file/v1".into();
        provider.api_key_env = "FILE_KEY".into();
        provider.openai.system_updates = SystemUpdates::Inline;
        provider.openai.reasoning_format = ReasoningFormat::Thinking;
        provider.openai.fill_missing_reasoning = true;
        provider.openai.prompt_cache_key = true;
        provider.openai.idle_timeout = Duration::from_secs(15);
        provider.retry.max_retries = 0;
        config
    }

    #[test]
    fn the_flag_and_the_variable_win_over_config_toml() {
        let settings = Settings::over(Some("flag-model"), Some("http://env/v1".into()), file(), None).unwrap();
        assert_eq!(settings.model, "flag-model");
        assert_eq!(settings.config.provider.openai.base_url, "http://env/v1");
    }

    #[test]
    fn config_toml_gives_every_other_setting() {
        let settings = Settings::over(None, None, file(), None).unwrap();
        assert_eq!(settings.model, "file-model");
        let provider = &settings.config.provider;
        assert_eq!(provider.openai.base_url, "http://file/v1");
        assert_eq!(provider.api_key_env, "FILE_KEY");
        assert_eq!(provider.openai.system_updates, SystemUpdates::Inline);
        assert_eq!(settings.config.reasoning, Some(Reasoning::Low));
        assert_eq!(provider.openai.reasoning_format, ReasoningFormat::Thinking);
        assert!(provider.openai.fill_missing_reasoning && provider.openai.prompt_cache_key);
        assert_eq!(settings.config.max_tokens, Some(512));
        assert_eq!(settings.tool_timeout, Duration::from_secs(30));
        assert_eq!(provider.openai.idle_timeout, Duration::from_secs(15));
        assert_eq!(provider.retry.max_retries, 0);
        assert_eq!(settings.approval_timeout, Duration::from_secs(60));
    }

    #[test]
    fn without_either_the_built_in_defaults_apply() {
        let settings = Settings::over(Some("m"), None, Config::default(), None).unwrap();
        let provider = &settings.config.provider;
        assert_eq!(provider.openai.base_url, "https://api.openai.com/v1");
        assert_eq!(provider.api_key_env, "OPENAI_API_KEY");
        assert_eq!(provider.openai.system_updates, SystemUpdates::Collapse);
        assert_eq!(settings.config.reasoning, None);
        assert_eq!(provider.openai.reasoning_format, ReasoningFormat::Effort);
        assert!(!provider.openai.fill_missing_reasoning && !provider.openai.prompt_cache_key);
        assert_eq!(settings.config.max_tokens, None);
        assert_eq!(settings.tool_timeout, Duration::from_secs(120));
        assert_eq!(provider.openai.idle_timeout, Duration::from_secs(120));
        assert_eq!(provider.retry.max_retries, 3);
        assert_eq!(settings.approval_timeout, Duration::from_secs(600));
    }

    #[test]
    fn without_a_model_anywhere_the_error_names_every_place_to_set_one() {
        let config = Config { model: None, ..file() };
        let error = format!("{:#}", Settings::over(None, None, config, None).err().unwrap());
        assert_eq!(
            error,
            "no model: pass --model, set ZX_MODEL, or set model in /home/me/.zx-agent/config.toml"
        );
    }
}
