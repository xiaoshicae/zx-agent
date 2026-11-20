//! The environment a process runs in: config.toml, with the model of the host and the
//! variables over it, and the secret files to redact.
//!
//! Where the environment is read, the model is the host's, else `ZX_MODEL`'s, else
//! config.toml's; the provider's address `ZX_BASE_URL`'s, else config.toml's, else the
//! provider's own default; and the files of `ZX_SECRET_FILES` are
//! redacted besides those of config.toml. A host that gives the configuration itself reads none
//! of these variables. Every other setting comes from config.toml or its built-in default.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use zx_agent::builtin::redact::check_secret_files;
use zx_util::env::env_value;

use crate::config::{self, Config};

/// The variable that names the model under the host's and over config.toml's.
const MODEL_ENV: &str = "ZX_MODEL";
/// The variable that names the provider's address over config.toml.
pub(super) const BASE_URL_ENV: &str = "ZX_BASE_URL";
/// Comma-separated secret files to redact, added to `secretFiles` of config.toml; deployments
/// set it, for example to the token that the platform mounts for the agent's own account.
const SECRET_FILES_ENV: &str = "ZX_SECRET_FILES";
/// Seconds a bash command may run, and an MCP call wait, unless config.toml says otherwise.
const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 120;
/// How long a person has to answer an approval by default, in seconds: long enough to notice a
/// card in an IM thread, short enough that a forgotten one does not hold a task.
const DEFAULT_APPROVAL_TIMEOUT_SECS: u64 = 600;

/// No model was given, and the configuration names none: the error of a start without a
/// model, which a host recognizes by downcasting, to name its own ways to give one.
#[derive(Debug, thiserror::Error)]
#[error("no model: none was given, and {config_file} sets no model")]
pub struct NoModel {
    /// The configuration that names no model: the path of config.toml, or `config.toml` when
    /// the configuration was not read from a file.
    pub config_file: String,
}

/// config.toml with the host's model and the variables over it, the defaults filled in.
pub(super) struct Settings {
    pub(super) model: String,
    pub(super) tool_timeout: Duration,
    /// How long a person has to answer an approval.
    pub(super) approval_timeout: Duration,
    /// The rest of config.toml: the provider, with `ZX_BASE_URL` over its address, the
    /// reasoning level, where commands run, the MCP servers, the models, the secret files and
    /// where long tool results go.
    pub(super) config: Config,
    /// The zx-agent home directory, whose `agents/` holds the user's agents.
    pub(super) home: Option<PathBuf>,
}

impl Settings {
    /// Read config.toml from the zx-agent home and lay `model`, the host's, else `ZX_MODEL`,
    /// and `ZX_BASE_URL` over it. A process without a model anywhere is a configuration error.
    pub(super) fn from_environment(model: Option<String>) -> Result<Self> {
        let home = config::home()?;
        let config = match &home {
            Some(home) => Config::load(&home.join(config::CONFIG_FILE))?,
            None => Config::default(),
        };
        let model = model.or_else(|| env_value(MODEL_ENV));
        Self::over(model, env_value(BASE_URL_ENV), config, home)
    }

    /// `model` and `base_url` over `config`, read from `home`. Every host comes here, so a
    /// configuration that was built rather than read is checked here as well.
    pub(super) fn over(
        model: Option<String>,
        base_url: Option<String>,
        mut config: Config,
        home: Option<PathBuf>,
    ) -> Result<Self> {
        config.validate().context("invalid configuration")?;
        let Some(model) = model.or_else(|| config.model.clone()) else {
            let config_file = config
                .path
                .as_ref()
                .map_or_else(|| config::CONFIG_FILE.to_string(), |path| path.display().to_string());
            return Err(NoModel { config_file }.into());
        };
        anyhow::ensure!(!model.trim().is_empty(), "the model must not be empty");
        if let Some(url) = base_url {
            config
                .provider
                .options
                .insert("baseUrl".into(), toml::Value::String(url));
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

/// The secret files to redact where the environment is read: `ZX_SECRET_FILES` and
/// `configured`, the `secretFiles` of config.toml, each checked at startup.
///
/// Both sources count: a file named in config.toml must not drop the ones a deployment
/// registered in the environment. A misspelt path would leave a secret unredacted without a
/// word, so each must exist at startup, and every file must be one the redaction can match;
/// one that disappears later is skipped, and one that becomes unusable later withholds the
/// output (`RedactSecrets::with_secret_files`). The notice about short values describes the
/// configuration, which stays the same while the process runs, so it is said once, at startup.
pub(super) fn secret_files(configured: &[PathBuf]) -> Result<Vec<PathBuf>> {
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
        .chain(configured.iter().cloned())
        .filter(|path| !path.as_os_str().is_empty())
        .collect();
    checked_secret_files(secret_files).with_context(|| format!("secretFiles in config.toml or {SECRET_FILES_ENV}"))
}

/// `secret_files`, checked: each exists, and each is a file the redaction can match. No
/// variable is read.
pub(super) fn checked_secret_files(secret_files: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    for path in &secret_files {
        std::fs::metadata(path).with_context(|| format!("cannot use secret file {}", path.display()))?;
    }
    check_secret_files(&secret_files).map_err(anyhow::Error::msg)?;
    Ok(secret_files)
}

#[cfg(test)]
mod tests {
    use zx_agent::ai::Reasoning;

    use super::*;
    use crate::config::OPENAI;

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
        provider.options = toml::from_str("baseUrl = 'http://file/v1'\nsystemUpdates = 'inline'").unwrap();
        provider.api_key_env = Some("FILE_KEY".into());
        provider.retry.max_retries = 0;
        config
    }

    #[test]
    fn the_hosts_model_and_the_variable_win_over_config_toml() {
        let settings = Settings::over(Some("host-model".into()), Some("http://env/v1".into()), file(), None).unwrap();
        assert_eq!(settings.model, "host-model");
        let options = &settings.config.provider.options;
        assert_eq!(options["baseUrl"].as_str(), Some("http://env/v1"));
        assert_eq!(options["systemUpdates"].as_str(), Some("inline"));
    }

    #[test]
    fn config_toml_gives_every_other_setting() {
        let settings = Settings::over(None, None, file(), None).unwrap();
        assert_eq!(settings.model, "file-model");
        let provider = &settings.config.provider;
        assert_eq!(provider.options["baseUrl"].as_str(), Some("http://file/v1"));
        assert_eq!(provider.api_key_env.as_deref(), Some("FILE_KEY"));
        assert_eq!(settings.config.reasoning, Some(Reasoning::Low));
        assert_eq!(settings.config.max_tokens, Some(512));
        assert_eq!(settings.tool_timeout, Duration::from_secs(30));
        assert_eq!(provider.retry.max_retries, 0);
        assert_eq!(settings.approval_timeout, Duration::from_secs(60));
    }

    #[test]
    fn without_either_the_built_in_defaults_apply() {
        let settings = Settings::over(Some("m".into()), None, Config::default(), None).unwrap();
        let provider = &settings.config.provider;
        assert_eq!(provider.api, OPENAI);
        assert!(provider.options.is_empty() && provider.api_key_env.is_none());
        assert_eq!(settings.config.reasoning, None);
        assert_eq!(settings.config.max_tokens, None);
        assert_eq!(settings.tool_timeout, Duration::from_secs(120));
        assert_eq!(provider.retry.max_retries, 3);
        assert_eq!(settings.approval_timeout, Duration::from_secs(600));
    }

    #[test]
    fn without_a_model_anywhere_the_error_names_the_configuration() {
        let config = Config { model: None, ..file() };
        let error = Settings::over(None, None, config, None).err().unwrap();
        assert_eq!(
            error
                .downcast_ref::<NoModel>()
                .map(|no_model| no_model.config_file.as_str()),
            Some("/home/me/.zx-agent/config.toml")
        );
    }
}
