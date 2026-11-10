//! The environment a process runs in: config.toml, with the model of the command line and
//! two variables over it.
//!
//! The model comes from `--model`, else `ZX_MODEL` (clap reads both), else config.toml; the
//! provider's address from `ZX_BASE_URL`, else config.toml, else the built-in default. Every
//! other setting comes from config.toml or its built-in default.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};

use zx_agent::ai::Reasoning;
use zx_agent::ai::openai::{ReasoningFormat, SystemUpdates};
use zx_agent::config::{self, Config};

use super::env_value;

/// The variable that names the provider's address over config.toml.
const BASE_URL_ENV: &str = "ZX_BASE_URL";
/// The provider's address when neither `ZX_BASE_URL` nor config.toml names one.
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
/// The variable holding the API key when config.toml names none.
const DEFAULT_API_KEY_ENV: &str = "OPENAI_API_KEY";
/// Seconds a bash command may run, and an MCP call wait, unless config.toml says otherwise.
const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 120;
/// Seconds to wait for response headers or the next stream chunk by default.
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 120;
/// Retries for transient provider errors by default.
const DEFAULT_MAX_RETRIES: u32 = 3;
/// How long chat waits for an approval by default, in seconds.
const DEFAULT_CHAT_APPROVAL_SECS: u64 = 120;
/// How long acp waits for the client's answer to a permission request by default, in seconds.
const DEFAULT_ACP_APPROVAL_SECS: u64 = 3600;

/// config.toml with the command line over it, the defaults filled in.
pub(crate) struct Settings {
    pub(crate) model: String,
    pub(crate) reasoning: Option<Reasoning>,
    pub(super) max_tokens: Option<u32>,
    pub(super) base_url: String,
    pub(super) api_key_env: String,
    pub(super) system_updates: SystemUpdates,
    pub(super) reasoning_format: ReasoningFormat,
    pub(super) fill_missing_reasoning: bool,
    pub(super) prompt_cache_key: bool,
    pub(super) request_fields: serde_json::Map<String, serde_json::Value>,
    pub(super) headers: Vec<(String, String)>,
    pub(super) idle_timeout: Duration,
    pub(super) max_retries: u32,
    pub(super) tool_timeout: Duration,
    /// How long chat waits for an approval.
    pub(crate) chat_approval_timeout: Duration,
    /// How long acp waits for the client's answer to a permission request.
    pub(crate) acp_approval_timeout: Duration,
    /// The rest of config.toml: where commands run, the MCP servers, the models, the secret
    /// files and where long tool results go.
    pub(super) config: Config,
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
    fn over(model: Option<&str>, base_url: Option<String>, config: Config, home: Option<PathBuf>) -> Result<Self> {
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
        let provider = &config.provider;
        let seconds = |value: Option<u64>, default| Duration::from_secs(value.unwrap_or(default));
        Ok(Self {
            model,
            reasoning: config.reasoning,
            max_tokens: config.max_tokens,
            base_url: base_url
                .or_else(|| provider.base_url.clone())
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            api_key_env: provider
                .api_key_env
                .clone()
                .unwrap_or_else(|| DEFAULT_API_KEY_ENV.to_string()),
            system_updates: provider.system_updates.unwrap_or_default(),
            reasoning_format: provider.reasoning_format.unwrap_or_default(),
            fill_missing_reasoning: provider.fill_missing_reasoning,
            prompt_cache_key: provider.prompt_cache_key,
            request_fields: provider.request_fields.clone(),
            headers: provider.headers.clone(),
            idle_timeout: seconds(provider.idle_timeout_secs, DEFAULT_IDLE_TIMEOUT_SECS),
            max_retries: provider.max_retries.unwrap_or(DEFAULT_MAX_RETRIES),
            tool_timeout: seconds(config.tool_timeout_secs, DEFAULT_TOOL_TIMEOUT_SECS),
            chat_approval_timeout: seconds(config.chat.approval_timeout_secs, DEFAULT_CHAT_APPROVAL_SECS),
            acp_approval_timeout: seconds(config.acp.approval_timeout_secs, DEFAULT_ACP_APPROVAL_SECS),
            config,
            home,
        })
    }
}

#[cfg(test)]
mod tests {
    use zx_agent::config::{FrontendSettings, ProviderSettings};

    use super::*;

    /// A config.toml that sets every key with a built-in default.
    fn file() -> Config {
        Config {
            path: Some(PathBuf::from("/home/me/.zx-agent/config.toml")),
            model: Some("file-model".into()),
            reasoning: Some(Reasoning::Low),
            max_tokens: Some(512),
            tool_timeout_secs: Some(30),
            provider: ProviderSettings {
                base_url: Some("http://file/v1".into()),
                api_key_env: Some("FILE_KEY".into()),
                system_updates: Some(SystemUpdates::Inline),
                reasoning_format: Some(ReasoningFormat::Thinking),
                fill_missing_reasoning: true,
                prompt_cache_key: true,
                idle_timeout_secs: Some(15),
                max_retries: Some(0),
                ..ProviderSettings::default()
            },
            chat: FrontendSettings {
                approval_timeout_secs: Some(60),
            },
            acp: FrontendSettings {
                approval_timeout_secs: Some(600),
            },
            ..Config::default()
        }
    }

    #[test]
    fn the_flag_and_the_variable_win_over_config_toml() {
        let settings = Settings::over(Some("flag-model"), Some("http://env/v1".into()), file(), None).unwrap();
        assert_eq!(settings.model, "flag-model");
        assert_eq!(settings.base_url, "http://env/v1");
    }

    #[test]
    fn config_toml_gives_every_other_setting() {
        let settings = Settings::over(None, None, file(), None).unwrap();
        assert_eq!(settings.model, "file-model");
        assert_eq!(settings.base_url, "http://file/v1");
        assert_eq!(settings.api_key_env, "FILE_KEY");
        assert_eq!(settings.system_updates, SystemUpdates::Inline);
        assert_eq!(settings.reasoning, Some(Reasoning::Low));
        assert_eq!(settings.reasoning_format, ReasoningFormat::Thinking);
        assert!(settings.fill_missing_reasoning && settings.prompt_cache_key);
        assert_eq!(settings.max_tokens, Some(512));
        assert_eq!(settings.tool_timeout, Duration::from_secs(30));
        assert_eq!(settings.idle_timeout, Duration::from_secs(15));
        assert_eq!(settings.max_retries, 0);
        assert_eq!(settings.chat_approval_timeout, Duration::from_secs(60));
        assert_eq!(settings.acp_approval_timeout, Duration::from_secs(600));
    }

    #[test]
    fn without_either_the_built_in_defaults_apply() {
        let settings = Settings::over(Some("m"), None, Config::default(), None).unwrap();
        assert_eq!(settings.base_url, "https://api.openai.com/v1");
        assert_eq!(settings.api_key_env, "OPENAI_API_KEY");
        assert_eq!(settings.system_updates, SystemUpdates::Collapse);
        assert_eq!(settings.reasoning, None);
        assert_eq!(settings.reasoning_format, ReasoningFormat::Effort);
        assert!(!settings.fill_missing_reasoning && !settings.prompt_cache_key);
        assert_eq!(settings.max_tokens, None);
        assert_eq!(settings.tool_timeout, Duration::from_secs(120));
        assert_eq!(settings.idle_timeout, Duration::from_secs(120));
        assert_eq!(settings.max_retries, 3);
        assert_eq!(settings.chat_approval_timeout, Duration::from_secs(120));
        assert_eq!(settings.acp_approval_timeout, Duration::from_secs(3600));
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
