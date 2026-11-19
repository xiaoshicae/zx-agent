//! Agents made from their configuration: config.toml, the agent file and what a host says over
//! them when it starts ([`Overrides`]), with the parts the host adds of its own. Every host
//! builds its agents here, through [`Application`], and none of this knows a command line, a
//! terminal or a protocol.
//!
//! - `settings`: config.toml with the host's model and the variables over it, and the secret
//!   files to redact
//! - `mcp`: the MCP servers of config.toml, connected once for every session of a process
//! - `provider`: the provider of an agent, which the factory registered under the name
//!   config.toml gives builds
//! - `agent`: the agent a host builds, and the sessions it creates or resumes with it
//! - `skills`: read_skill, which the agent registers for its skills
//! - `setup`: what each session gets for itself: its executor, the tools on it, its store of
//!   long tool results and the prompt sections about its directory
//! - `execution`: where a session's commands run: the executor factory registered under the
//!   name config.toml gives, this machine or a Docker container unless a host registers another
//! - `place`: where a host keeps the sessions: memory, one file, or the home directory

mod agent;
mod execution;
mod mcp;
mod place;
mod provider;
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
use zx_agent::compaction::Summarizer;
use zx_agent::extensions::Extension;
use zx_agent::hooks::Hook;
use zx_agent::policy::ToolPolicy;
use zx_agent::tools::Tool;
use zx_util::registry::Registry;

pub use agent::{Approvals, ConfiguredAgent};
pub use execution::{ExecutorFactory, ExecutorSession};
pub use place::SessionPlace;
pub use provider::{BuiltProvider, ProviderFactory, ProviderInput};
pub use settings::NoModel;

use mcp::McpTools;
use settings::{Settings, checked_secret_files, secret_files};

/// What the agents of a process share, read and connected once: the settings with what the
/// host says over them, the MCP servers, the secret files to redact, the variables commands
/// must not see and the trace export. It builds a [`ConfiguredAgent`] for each place sessions
/// are kept in, the host's own storage among them ([`SessionPlace::from_storage`]); no
/// session is opened at startup.
///
/// A host gives its own parts with three verbs: `add_*` adds one beside the assembly's
/// (tools, hooks, policies, extensions, prompt sections), `set_*` replaces the one there is
/// (the summarizer, the hidden variables, the trace export), and `register_*` names one
/// that config.toml chooses by its name (providers, executors). They are treated as the built-in ones
/// are, and a name that another part goes by already fails [`build`](Self::build).
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
    /// The prompt sections the host adds to every session, as `(name, content)`.
    host_sections: Vec<(String, String)>,
    /// The host's own tools, offered where the agent enables them, as the built-in ones are.
    tools: Vec<Arc<dyn Tool>>,
    /// The host's own hooks and policies, by name, after the assembly's.
    hooks: Vec<(String, Arc<dyn Hook>)>,
    policies: Vec<(String, Arc<dyn ToolPolicy>)>,
    /// The host's own extensions, after the MCP servers and treated as they are.
    extensions: Vec<Arc<dyn Extension>>,
    /// What makes the summaries in place of the agent's model, when the host gives it.
    summarizer: Option<Arc<dyn Summarizer>>,
    /// The providers config.toml can name in `provider.api`, and the executors it can name in
    /// `execution`: the built-in ones, then the host's.
    providers: Registry<dyn ProviderFactory>,
    executors: Registry<dyn ExecutorFactory>,
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
        let executors = execution::built_in(&settings.config.container);
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
            host_sections: Vec::new(),
            tools: Vec::new(),
            hooks: Vec::new(),
            policies: Vec::new(),
            extensions: Vec::new(),
            summarizer: None,
            providers: provider::built_in(),
            executors,
            file_queue: Arc::default(),
        })
    }

    /// Hide `names` from commands and redact their values: credentials besides the API key,
    /// such as the tokens of an IM service's platforms.
    pub fn set_hidden_env(mut self, names: Vec<String>) -> Self {
        self.hidden_env = names;
        self
    }

    /// Export the runs of every session through `traces`, the host's trace hook, when it
    /// exports them. The host starts and finishes the export
    /// ([`with_telemetry`](crate::shutdown::with_telemetry)); nothing here installs it.
    pub fn set_traces(mut self, traces: Option<TraceHook>) -> Self {
        self.traces = traces;
        self
    }

    /// Add the section `name` to the prompt of every session, after those the assembly
    /// gives: what the model should know of the host itself, such as how its answers reach
    /// people. The agent's instructions name no host, so that one agent serves them all.
    pub fn add_prompt_section(mut self, name: &str, content: &str) -> Self {
        self.host_sections.push((name.into(), content.into()));
        self
    }

    /// Add `tool`, offered where the agent's `tools` enables it and asked about where its
    /// `permissions.ask` names it, as a built-in tool is. Like one, it must bound its own
    /// output and stop when its call is cancelled; redaction applies to what it returns, and
    /// so does the store of long results where the agent enables read_output.
    pub fn add_tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.push(tool);
        self
    }

    /// Add `hook` as `name`, after the assembly's hooks of the stage it declares. Redaction
    /// runs after every hook of the default stage, so a hook cannot bring a secret back.
    pub fn add_hook(mut self, name: &str, hook: Arc<dyn Hook>) -> Self {
        self.hooks.push((name.into(), hook));
        self
    }

    /// Add `policy` as `name`. The strictest verdict of all policies stands, so a policy can
    /// tighten what the agent's permissions allow but never loosen it.
    pub fn add_policy(mut self, name: &str, policy: Arc<dyn ToolPolicy>) -> Self {
        self.policies.push((name.into(), policy));
        self
    }

    /// Add `extension` after the MCP servers, which are extensions too, and treated as they
    /// are: its tools are offered to every agent and ask unless `permissions.allow` names
    /// them.
    pub fn add_extension(mut self, extension: Arc<dyn Extension>) -> Self {
        self.extensions.push(extension);
        self
    }

    /// Register `factory` under `name`, the value of `provider.api` that chooses it. `openai`
    /// and `anthropic` are built in; it builds the provider from the keys of `[provider]`
    /// that zx-app does not read itself, and checks them.
    pub fn register_provider(mut self, name: &str, factory: Arc<dyn ProviderFactory>) -> Self {
        self.providers.register(name, factory);
        self
    }

    /// Register `factory` under `name`, the value of `execution` that chooses it. `local` and
    /// `container` are built in; it starts the executor of each session, which the agent's
    /// tools that run commands and touch files work through.
    pub fn register_executor(mut self, name: &str, factory: Arc<dyn ExecutorFactory>) -> Self {
        self.executors.register(name, factory);
        self
    }

    /// Make the summaries with `summarizer` in place of the agent's model.
    pub fn set_summarizer(mut self, summarizer: Arc<dyn Summarizer>) -> Self {
        self.summarizer = Some(summarizer);
        self
    }

    /// Assemble the agent for `model`, usually [`Self::model`], whose sessions are kept in
    /// `place`, sharing what startup read and connected with the host's other agents. Startup
    /// reads; each session gets its executor when it is created or opened.
    ///
    /// Fails on an agent file or a tool setting that cannot be used, a cost budget without
    /// prices, invalid provider settings, a provider or an executor that no factory is
    /// registered for, and a part of the host's whose name another part goes by already.
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
