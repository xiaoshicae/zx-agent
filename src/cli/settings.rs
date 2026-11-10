//! The environment a process runs in: config.toml, with the command line and its variables
//! over it, key by key.
//!
//! For each key the command line wins, then its variable where it has one (`ZX_MODEL`,
//! `ZX_BASE_URL`; clap reads both), then config.toml, then the built-in default. The two
//! switches `--fill-missing-reasoning` and `--prompt-cache-key` only turn a setting on.

use std::path::PathBuf;

use anyhow::{Context, Result};

use zx_agent::ai::Reasoning;
use zx_agent::ai::openai::{ReasoningFormat, SystemUpdates};
use zx_agent::config::{self, Config};

use super::{BaseArgs, RequestArgs};

/// The provider's address when neither the command line nor config.toml names one.
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
/// The variable holding the API key when neither the command line nor config.toml names one.
const DEFAULT_API_KEY_ENV: &str = "OPENAI_API_KEY";

/// config.toml with the command line over it.
pub(crate) struct Settings {
    pub(crate) model: String,
    pub(crate) reasoning: Option<Reasoning>,
    pub(super) base_url: String,
    pub(super) api_key_env: String,
    pub(super) system_updates: SystemUpdates,
    pub(super) reasoning_format: ReasoningFormat,
    pub(super) fill_missing_reasoning: bool,
    pub(super) prompt_cache_key: bool,
    /// The rest of config.toml: where commands run, the MCP servers, the models and where
    /// long tool results go.
    pub(super) config: Config,
    /// The zx-agent home directory, whose `agents/` holds the user's agents.
    pub(super) home: Option<PathBuf>,
}

impl Settings {
    /// Read config.toml from the zx-agent home and lay the command line over it. A process
    /// without a model anywhere is a configuration error.
    pub(super) fn resolve(base: &BaseArgs, request: &RequestArgs) -> Result<Self> {
        let home = config::home()?;
        let config = match &home {
            Some(home) => Config::load(&home.join("config.toml"))?,
            None => Config::default(),
        };
        Self::over(base, request, config, home)
    }

    /// The command line over `config`, read from `home`.
    fn over(base: &BaseArgs, request: &RequestArgs, config: Config, home: Option<PathBuf>) -> Result<Self> {
        let model = base.model.clone().or_else(|| config.model.clone()).with_context(|| {
            let place = config
                .path
                .as_ref()
                .map_or_else(|| "config.toml".to_string(), |path| path.display().to_string());
            format!("no model: pass --model, set ZX_MODEL, or set model in {place}")
        })?;
        anyhow::ensure!(!model.trim().is_empty(), "the model is empty");
        let provider = &config.provider;
        Ok(Self {
            model,
            reasoning: request.reasoning.or(config.reasoning),
            base_url: base
                .base_url
                .clone()
                .or_else(|| provider.base_url.clone())
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            api_key_env: base
                .api_key_env
                .clone()
                .or_else(|| provider.api_key_env.clone())
                .unwrap_or_else(|| DEFAULT_API_KEY_ENV.to_string()),
            system_updates: base.system_updates.or(provider.system_updates).unwrap_or_default(),
            reasoning_format: request
                .reasoning_format
                .or(provider.reasoning_format)
                .unwrap_or_default(),
            fill_missing_reasoning: request.fill_missing_reasoning || provider.fill_missing_reasoning,
            prompt_cache_key: request.prompt_cache_key || provider.prompt_cache_key,
            config,
            home,
        })
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use zx_agent::config::ProviderSettings;

    use super::*;
    use crate::cli::{Cli, Command, RunArgs};

    /// `run` with `args` and a prompt. Only the flags given here count: clap would also read
    /// the variables `ZX_MODEL` and `ZX_BASE_URL` of the test process.
    fn run_args(args: &[&str]) -> RunArgs {
        let cli = Cli::try_parse_from(["zx-agent", "run"].iter().chain(args).chain(&["go"])).unwrap();
        let Command::Run(mut run) = cli.command else {
            panic!("run command")
        };
        if !args.contains(&"--model") {
            run.base.model = None;
        }
        if !args.contains(&"--base-url") {
            run.base.base_url = None;
        }
        run
    }

    fn settings(args: &[&str], config: Config) -> Result<Settings> {
        let run = run_args(args);
        Settings::over(&run.base, &run.request, config, None)
    }

    /// A config.toml that sets every key the command line also takes.
    fn file() -> Config {
        Config {
            path: Some(PathBuf::from("/home/me/.zx-agent/config.toml")),
            model: Some("file-model".into()),
            reasoning: Some(Reasoning::Low),
            provider: ProviderSettings {
                base_url: Some("http://file/v1".into()),
                api_key_env: Some("FILE_KEY".into()),
                system_updates: Some(SystemUpdates::Inline),
                reasoning_format: Some(ReasoningFormat::Thinking),
                fill_missing_reasoning: true,
                prompt_cache_key: false,
            },
            ..Config::default()
        }
    }

    #[test]
    fn the_command_line_wins_over_config_toml() {
        let settings = settings(
            &[
                "--model",
                "flag-model",
                "--base-url",
                "http://flag/v1",
                "--api-key-env",
                "FLAG_KEY",
                "--system-updates",
                "collapse",
                "--reasoning",
                "high",
                "--reasoning-format",
                "effort",
            ],
            file(),
        )
        .unwrap();
        assert_eq!(settings.model, "flag-model");
        assert_eq!(settings.base_url, "http://flag/v1");
        assert_eq!(settings.api_key_env, "FLAG_KEY");
        assert_eq!(settings.system_updates, SystemUpdates::Collapse);
        assert_eq!(settings.reasoning, Some(Reasoning::High));
        assert_eq!(settings.reasoning_format, ReasoningFormat::Effort);
    }

    #[test]
    fn config_toml_fills_what_the_command_line_leaves_out() {
        let settings = settings(&[], file()).unwrap();
        assert_eq!(settings.model, "file-model");
        assert_eq!(settings.base_url, "http://file/v1");
        assert_eq!(settings.api_key_env, "FILE_KEY");
        assert_eq!(settings.system_updates, SystemUpdates::Inline);
        assert_eq!(settings.reasoning, Some(Reasoning::Low));
        assert_eq!(settings.reasoning_format, ReasoningFormat::Thinking);
    }

    #[test]
    fn without_either_the_built_in_defaults_apply() {
        let settings = settings(&["--model", "m"], Config::default()).unwrap();
        assert_eq!(settings.base_url, "https://api.openai.com/v1");
        assert_eq!(settings.api_key_env, "OPENAI_API_KEY");
        assert_eq!(settings.system_updates, SystemUpdates::Collapse);
        assert_eq!(settings.reasoning, None);
        assert_eq!(settings.reasoning_format, ReasoningFormat::Effort);
        assert!(!settings.fill_missing_reasoning && !settings.prompt_cache_key);
    }

    #[test]
    fn the_switches_are_on_when_either_side_turns_them_on() {
        let settings = settings(&["--prompt-cache-key"], file()).unwrap();
        assert!(settings.prompt_cache_key);
        assert!(settings.fill_missing_reasoning);
    }

    #[test]
    fn without_a_model_anywhere_the_error_names_every_place_to_set_one() {
        let config = Config { model: None, ..file() };
        let error = format!("{:#}", settings(&[], config).err().unwrap());
        assert_eq!(
            error,
            "no model: pass --model, set ZX_MODEL, or set model in /home/me/.zx-agent/config.toml"
        );
    }
}
