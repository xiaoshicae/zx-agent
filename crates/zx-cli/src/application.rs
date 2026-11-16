//! Configured agents for a host, independent of argument parsing, terminals and protocols.
//!
//! [`Application::from_config`] accepts explicit environment configuration and an optional
//! agent-file directory. [`Application::from_environment`] opts into the command line's
//! config discovery and process overrides. Both share MCP connections across agents and
//! give each session its own executor through the same assembly used by the CLI.
//!
//! Startup notices are returned to the host, never printed. Credentials named by the
//! configuration still come from environment variables. This boundary lives in `zx-cli`
//! until a second host justifies extracting it together with `config` and `otel`.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;

use crate::assembly::{Settings, Shared, checked_secret_files};
use crate::config::{Builtin, Config};
use crate::otel::TraceHook;

#[doc(inline)]
pub use crate::assembly::{Approvals, ModeAgent as ConfiguredAgent, Overrides, SessionPlace};

/// The shared resources of configured agents: resolved settings, MCP connections,
/// redaction configuration and optional trace observation. No sessions are opened at startup.
pub struct Application {
    shared: Shared,
}

impl Application {
    /// Connect using explicit `config`, with `overrides` over it. `home` is only the root
    /// for agent files; no config.toml is read from it. `ZX_BASE_URL`, `ZX_MODEL` and
    /// `ZX_SECRET_FILES` are ignored. Named API/MCP credentials still come from the process.
    /// Invalid configuration, unavailable MCP servers and unusable secret files fail startup.
    pub async fn from_config(
        config: Config,
        home: Option<PathBuf>,
        overrides: Overrides,
        role: Builtin,
    ) -> Result<Self> {
        let settings = Settings::over(overrides.model.as_deref(), None, config, home)?;
        let files = checked_secret_files(settings.config.secret_files.clone())?;
        Ok(Self {
            shared: Shared::configured(settings, overrides, role, files).await?,
        })
    }

    /// Opt into config.toml discovery, `ZX_BASE_URL`, `ZX_SECRET_FILES` and explicit
    /// overrides. Model selection is `overrides.model`, then config.toml; a host wanting
    /// `ZX_MODEL` resolves it into `overrides`, as the CLI does.
    pub async fn from_environment(overrides: Overrides, role: Builtin) -> Result<Self> {
        Ok(Self {
            shared: Shared::from_environment(overrides, role).await?,
        })
    }

    /// Hide additional credential variables from local commands and redact their values.
    pub fn with_hidden_env(mut self, names: Vec<String>) -> Self {
        self.shared = self.shared.with_hidden_env(names);
        self
    }

    /// Observe runs with the host's trace hook. The host owns exporter initialization
    /// and shutdown; constructing the application does not install global telemetry.
    pub fn with_traces(mut self, traces: Option<TraceHook>) -> Self {
        self.shared = self.shared.with_traces(traces);
        self
    }

    /// Assemble the selected role in `place`, sharing startup resources with other agents.
    /// Per-session executors are created only when a session is created or opened.
    pub fn build(&self, place: &SessionPlace, approvals: Approvals<'_>) -> Result<ConfiguredAgent> {
        ConfiguredAgent::build(&self.shared, &self.shared.settings.model, place, approvals)
    }

    /// The resolved model ID.
    pub fn model(&self) -> &str {
        &self.shared.settings.model
    }

    /// The configured time a host allows for an approval.
    pub fn approval_timeout(&self) -> Duration {
        self.shared.settings.approval_timeout
    }

    /// Sanitized startup notices, for the host to display or log if desired.
    pub fn notices(&self) -> &[String] {
        &self.shared.notices
    }
}
