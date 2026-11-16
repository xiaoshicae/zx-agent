//! The environment a process runs in: config.toml, with the model of the command line and
//! two variables over it.
//!
//! The model comes from the caller (the command line gives `--model`, else `ZX_MODEL`), else config.toml; the
//! provider's address from `ZX_BASE_URL`, else config.toml, else the provider's default
//! (`OpenAiConfig::default()`). Every other setting comes from config.toml or its built-in
//! default.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::{self, Config};

use super::env_value;

/// The variable that names the provider's address over config.toml.
const BASE_URL_ENV: &str = "ZX_BASE_URL";
/// Seconds a bash command may run, and an MCP call wait, unless config.toml says otherwise.
const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 120;
/// How long chat, acp and im wait for an approval by default, in seconds: long enough to
/// notice a card in an IM thread, short enough that a forgotten one does not hold a task.
const DEFAULT_APPROVAL_TIMEOUT_SECS: u64 = 600;

/// No model was given, and the configuration names none: the error of a start without a
/// model, which a host recognizes by downcasting. `place` is where the configuration was
/// read; a command line names its own ways to give a model instead.
#[derive(Debug)]
pub struct NoModel {
    pub place: String,
}

impl std::fmt::Display for NoModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no model: none was given, and {} sets no model", self.place)
    }
}

impl std::error::Error for NoModel {}

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

    /// `model` and `base_url` over `config`, read from `home`. Every way in comes here, so
    /// a configuration that was built rather than read is checked here as well.
    pub(crate) fn over(
        model: Option<&str>,
        base_url: Option<String>,
        mut config: Config,
        home: Option<PathBuf>,
    ) -> Result<Self> {
        config.validate().context("invalid configuration")?;
        let Some(model) = model.map(str::to_string).or_else(|| config.model.clone()) else {
            let place = config
                .path
                .as_ref()
                .map_or_else(|| "config.toml".to_string(), |path| path.display().to_string());
            return Err(NoModel { place }.into());
        };
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
        provider.openai.body.system_updates = SystemUpdates::Inline;
        provider.openai.body.reasoning_format = ReasoningFormat::Thinking;
        provider.openai.body.fill_missing_reasoning = true;
        provider.openai.body.prompt_cache_key = true;
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
        assert_eq!(provider.openai.body.system_updates, SystemUpdates::Inline);
        assert_eq!(settings.config.reasoning, Some(Reasoning::Low));
        assert_eq!(provider.openai.body.reasoning_format, ReasoningFormat::Thinking);
        assert!(provider.openai.body.fill_missing_reasoning && provider.openai.body.prompt_cache_key);
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
        assert_eq!(provider.openai.body.system_updates, SystemUpdates::Collapse);
        assert_eq!(settings.config.reasoning, None);
        assert_eq!(provider.openai.body.reasoning_format, ReasoningFormat::Effort);
        assert!(!provider.openai.body.fill_missing_reasoning && !provider.openai.body.prompt_cache_key);
        assert_eq!(settings.config.max_tokens, None);
        assert_eq!(settings.tool_timeout, Duration::from_secs(120));
        assert_eq!(provider.openai.idle_timeout, Duration::from_secs(120));
        assert_eq!(provider.retry.max_retries, 3);
        assert_eq!(settings.approval_timeout, Duration::from_secs(600));
    }

    #[test]
    fn without_a_model_anywhere_the_error_names_the_configuration() {
        let config = Config { model: None, ..file() };
        let error = Settings::over(None, None, config, None).err().unwrap();
        assert_eq!(
            error.downcast_ref::<NoModel>().map(|no_model| no_model.place.as_str()),
            Some("/home/me/.zx-agent/config.toml")
        );
    }
}
