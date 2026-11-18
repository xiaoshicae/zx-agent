//! Agents made from their configuration: config.toml, the agent file and what a host says over
//! them when it starts ([`Overrides`]). Every host builds its agents here, through
//! [`Application`], and none of this knows a command line, a terminal or a protocol.
//!
//! - `settings`: config.toml with the host's model and the variables over it, and the secret
//!   files to redact
//! - `mcp`: the MCP servers of config.toml, connected once for every session of a process
//! - `agent`: the agent a host builds, and the sessions it creates or resumes with it
//! - `skills`: read_skill, which the agent registers for its skills
//! - `setup`: what each session gets for itself: its executor, the tools on it, its store of
//!   long tool results and the prompt sections about its directory
//! - `execution`: where a session's commands run: this machine or a Docker container
//! - `place`: where a host keeps the sessions: memory, one file, or the home directory

mod agent;
mod execution;
mod mcp;
mod place;
mod settings;
mod setup;
mod skills;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::config::{Builtin, Config, cost_budget};
use crate::otel::TraceHook;
use zx_agent::ai::Reasoning;
use zx_agent::builtin::redact::{MIN_SECRET_CHARS, short_secret_files};
use zx_agent::builtin::tools::FileMutationQueue;

pub use agent::{Approvals, ConfiguredAgent};
pub use place::SessionPlace;
pub use settings::NoModel;

use mcp::McpTools;
use settings::{Settings, checked_secret_files, secret_files};

/// What the agents of a process share, read and connected once: the settings with what the
/// host says over them, the MCP servers, the secret files to redact, the variables commands
/// must not see and the trace export. It builds a [`ConfiguredAgent`] for each place sessions
/// are kept in; no session is opened at startup.
pub struct Application {
    settings: Settings,
    /// What the host says over the agent: the working directory, the tools and the budgets.
    overrides: Overrides,
    /// The built-in agent every session runs, or the user's file that replaces it.
    builtin: Builtin,
    mcp: McpTools,
    /// The files whose contents are redacted from tool output.
    secret_files: Vec<PathBuf>,
    /// What startup has to say, for the host to show: the MCP servers and short secrets.
    notices: Vec<String>,
    /// Variables that hold credentials besides the API key, such as an IM service's tokens:
    /// commands do not inherit them, and their values are redacted like the key's.
    hidden_env: Vec<String>,
    /// The hook that exports every session's runs as traces, when the host exports them.
    traces: Option<TraceHook>,
    /// The queue of changes to each file, which the file tools of every session wait in, so
    /// that two sessions never overwrite each other's edit.
    file_queue: Arc<FileMutationQueue>,
}

impl Application {
    /// Connect with `config` as the host gives it, with `overrides` over it, for sessions of
    /// `builtin`. `home` is only where the agent files are (`agents/`): no config.toml is read
    /// from it, and neither `ZX_MODEL`, `ZX_BASE_URL` nor `ZX_SECRET_FILES` is read. The
    /// credentials the configuration names still come from their variables.
    ///
    /// Fails on a configuration that [`Config::validate`] refuses, without a model
    /// ([`NoModel`]), on invalid [`Overrides`], an MCP server that cannot be used, and a secret
    /// file that cannot.
    pub async fn from_config(
        config: Config,
        home: Option<PathBuf>,
        overrides: Overrides,
        builtin: Builtin,
    ) -> Result<Self> {
        let settings = Settings::over(overrides.model.clone(), None, config, home)?;
        let secret_files = checked_secret_files(settings.config.secret_files.clone())?;
        Self::connect(settings, secret_files, overrides, builtin).await
    }

    /// Connect with the environment's configuration: config.toml in the zx-agent home
    /// ([`config::home`](crate::config::home)), with [`Overrides::model`], else `ZX_MODEL`,
    /// over its model, `ZX_BASE_URL` over its provider's address, and the files of
    /// `ZX_SECRET_FILES` redacted besides its own; with `overrides` over the agent, for
    /// sessions of `builtin`.
    ///
    /// Fails as [`from_config`](Self::from_config) does, and when the home directory or
    /// config.toml cannot be read.
    pub async fn from_environment(overrides: Overrides, builtin: Builtin) -> Result<Self> {
        let settings = Settings::from_environment(overrides.model.clone())?;
        let secret_files = secret_files(&settings.config.secret_files)?;
        Self::connect(settings, secret_files, overrides, builtin).await
    }

    /// What both ways of starting share once `settings` and the `secret_files` are checked:
    /// check `overrides`, connect the MCP servers, and note what startup has to say.
    async fn connect(
        settings: Settings,
        secret_files: Vec<PathBuf>,
        overrides: Overrides,
        builtin: Builtin,
    ) -> Result<Self> {
        overrides.validate()?;
        let (mcp, mut notices) = McpTools::connect(&settings).await?;
        for file in short_secret_files(&secret_files) {
            notices.push(format!(
                "[secret] {}: the value is shorter than {MIN_SECRET_CHARS} characters; it is still redacted, and so is every other occurrence of the same text in tool output",
                file.display()
            ));
        }
        Ok(Self {
            settings,
            overrides,
            builtin,
            mcp,
            secret_files,
            notices,
            hidden_env: Vec::new(),
            traces: None,
            file_queue: Arc::default(),
        })
    }

    /// Hide `names` from commands and redact their values: credentials besides the API key,
    /// such as the tokens of an IM service's platforms.
    pub fn with_hidden_env(mut self, names: Vec<String>) -> Self {
        self.hidden_env = names;
        self
    }

    /// Export the runs of every session through `traces`, the host's trace hook, when it
    /// exports them. The host starts and finishes the export
    /// ([`with_telemetry`](crate::shutdown::with_telemetry)); nothing here installs it.
    pub fn with_traces(mut self, traces: Option<TraceHook>) -> Self {
        self.traces = traces;
        self
    }

    /// Assemble the agent for `model`, usually [`Self::model`], whose sessions are kept in
    /// `place`, sharing what startup read and connected with the host's other agents. Startup
    /// reads; each session gets its executor when it is created or opened.
    ///
    /// Fails on an agent file or a tool setting that cannot be used, a cost budget without
    /// prices, and invalid provider settings.
    pub fn build(&self, model: &str, place: &SessionPlace, approvals: Approvals<'_>) -> Result<ConfiguredAgent> {
        ConfiguredAgent::build(self, model, place, approvals)
    }

    /// The model ID: the host's, else `ZX_MODEL`'s where the environment is read, else
    /// config.toml's.
    pub fn model(&self) -> &str {
        &self.settings.model
    }

    /// The reasoning level config.toml sets; `None` when it sets none.
    pub fn reasoning(&self) -> Option<Reasoning> {
        self.settings.config.reasoning
    }

    /// Where new sessions work unless a request names a directory: [`Overrides::cwd`];
    /// `None` for the current directory.
    pub fn cwd(&self) -> Option<&Path> {
        self.overrides.cwd.as_deref()
    }

    /// What startup has to say, without secrets, for the host to show or log: the MCP servers
    /// it connected and the secret files whose values are short.
    pub fn notices(&self) -> &[String] {
        &self.notices
    }
}

/// What a host says over config.toml and the agent when it starts, such as a program's
/// arguments: `None` keeps their value.
#[derive(Default)]
pub struct Overrides {
    /// The model ID, over `ZX_MODEL` and the `model` of config.toml.
    pub model: Option<String>,
    /// Where new sessions work; the current directory without it.
    pub cwd: Option<PathBuf>,
    /// The enabled tools, in place of the agent's.
    pub tools: Option<Vec<String>>,
    /// Model requests per run, failed attempts included.
    pub max_model_requests: Option<u32>,
    /// Wall-clock limit per run, in seconds; 0 removes the agent's.
    pub timeout_secs: Option<u64>,
    /// Tokens per run; 0 removes the agent's.
    pub token_budget: Option<u64>,
    /// Spending per run, in the unit of the configured prices; 0 removes the agent's. As in an
    /// agent file, an amount that is negative or not finite is refused.
    pub cost_budget: Option<f64>,
}

impl Overrides {
    /// Refuse what an agent file may not say either: a cost budget that is negative or not
    /// finite. The tools are checked with the agent they replace.
    fn validate(&self) -> Result<()> {
        if let Some(amount) = self.cost_budget {
            cost_budget(amount)?;
        }
        Ok(())
    }
}

/// The working directory of a new session, which is what its header records: `cwd`, or the
/// current directory, by its real path. Local and container execution record a directory
/// the same way, and the container mounts the workspace at that path.
///
/// Fails when the directory cannot be resolved or is not a directory.
pub fn resolve_cwd(cwd: Option<&Path>) -> Result<PathBuf> {
    let asked = match cwd {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().context("cannot find the current directory")?,
    };
    let cwd = asked
        .canonicalize()
        .with_context(|| format!("cannot resolve the working directory {}", asked.display()))?;
    anyhow::ensure!(
        cwd.is_dir(),
        "the working directory is not a directory: {}",
        cwd.display()
    );
    Ok(cwd)
}
