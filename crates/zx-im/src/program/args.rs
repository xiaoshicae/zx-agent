//! The command line of `zx-im`. Everything that describes the environment rather than one
//! invocation is in config.toml (`zx_app::config::Config`), the service's own settings are in
//! im.toml; what the command line says over the agent and config.toml is [`Overrides`].

use std::path::PathBuf;

use clap::Parser;
use zx_app::{Asking, Overrides};

#[derive(Parser)]
#[command(
    name = "zx-im",
    version,
    about = "Answer chat messages from IM platforms, one session per conversation, until stopped"
)]
pub(crate) struct Args {
    /// IM service configuration (TOML): platforms, who may use the bot, limits and the session
    /// directory. Not config.toml; see docs/CONFIGURATION.md, section "im.toml".
    #[arg(long)]
    config: PathBuf,

    // The options from here on are those of zx-agent's `run` and `chat`, with the same help:
    // zx-app does not use clap, and the two programs share no code.
    /// Model id passed to the provider. Defaults to ZX_MODEL, else model in config.toml.
    #[arg(long)]
    model: Option<String>,

    /// Working directory for tools. Defaults to the current directory.
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Enabled tools, comma-separated. Defaults to the agent's; explicit values replace them.
    #[arg(long, value_delimiter = ',')]
    tools: Option<Vec<String>>,

    /// Maximum model requests per prompt, failed attempts included. Defaults to the agent's.
    #[arg(long)]
    max_model_requests: Option<u32>,

    /// Wall-clock limit per prompt, in seconds. 0 disables it. Defaults to the agent's.
    #[arg(long)]
    timeout_secs: Option<u64>,

    /// Tokens per prompt: input plus output of its model requests, as the provider reports
    /// them. Checked before each request, so the last one may go over. 0 disables it.
    /// Defaults to the agent's.
    #[arg(long)]
    token_budget: Option<u64>,

    /// Spending per prompt, in the unit of the prices in [[models]] of config.toml (any
    /// currency, as long as every price uses it): the cost of its model requests by those
    /// prices. Checked before each request, so the last one may go over. The model must have
    /// prices. 0 disables it. Defaults to the agent's.
    #[arg(long)]
    cost_budget: Option<f64>,
}

impl Args {
    /// The IM service configuration file, and what the other options say over the agent and
    /// config.toml, as the application takes it and checks it. A run is one prompt: the
    /// messages one run answers.
    pub(super) fn into_parts(self) -> (PathBuf, Overrides) {
        let Self {
            config,
            model,
            cwd,
            tools,
            max_model_requests,
            timeout_secs,
            token_budget,
            cost_budget,
        } = self;
        let overrides = Overrides {
            model,
            // The approvers in the chat answer what asks: zx-im never skips them.
            asking: Asking::AsConfigured,
            cwd,
            tools,
            max_model_requests,
            timeout_secs,
            token_budget,
            cost_budget,
        };
        (config, overrides)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clap::CommandFactory;

    use super::*;

    #[test]
    fn takes_a_configuration_file_and_the_shared_settings() {
        let parts = |args: &[&str]| Args::try_parse_from(["zx-im"].iter().chain(args)).map(Args::into_parts);
        let (config, overrides) = parts(&["--config", "im.toml", "--model", "m"]).unwrap();
        assert_eq!(config, Path::new("im.toml"));
        assert_eq!(overrides.max_model_requests, None);
        let (_, overrides) = parts(&["--config", "im.toml", "--model", "m", "--max-model-requests", "3"]).unwrap();
        assert_eq!(overrides.max_model_requests, Some(3));
        assert!(Args::try_parse_from(["zx-im", "--model", "m"]).is_err());
        // Each conversation has a file of its own.
        assert!(Args::try_parse_from(["zx-im", "--config", "im.toml", "--session", "s.jsonl"]).is_err());
    }

    #[test]
    fn cwd_is_the_working_directory_of_tools() {
        let command = Args::command();
        let cwd = command.get_arguments().find(|arg| arg.get_id() == "cwd").unwrap();
        assert_eq!(
            cwd.get_help().unwrap().to_string(),
            "Working directory for tools. Defaults to the current directory"
        );
    }
}
