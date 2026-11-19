//! Where a session's commands run: the factory registered under the name config.toml gives in
//! `execution` starts an executor for each session. `local`, this machine, and
//! `container`, a Docker container for each session, are built in, the latter with the checks
//! it needs and its failures in the terms of config.toml; a host registers others with
//! [`Application::register_executor`](super::Application::register_executor).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;

use crate::config::{CONTAINER, ContainerSettings, LOCAL};
use zx_agent::builtin::executor::{ContainerConfig, ContainerExecutor, Executor, LocalExecutor, UnusableWorkspace};
use zx_util::registry::Registry;

/// Starts the executor each session runs its commands with, for the `execution` of
/// config.toml that names it.
#[async_trait]
pub trait ExecutorFactory: Send + Sync {
    /// Whether commands run apart from this machine, in a sandbox: only then may an agent
    /// approve every call (`permissions.approveAll`). They do not unless the factory says so.
    fn isolates(&self) -> bool {
        false
    }

    /// The executor of `session`, started when the session is created or opened and kept
    /// until it is closed. Fails when it cannot start.
    async fn start(&self, session: ExecutorSession<'_>) -> Result<Arc<dyn Executor>>;
}

/// The session an executor is started for.
pub struct ExecutorSession<'a> {
    /// Its working directory, which exists.
    pub cwd: &'a Path,
    /// Its ID, to label what is started for it.
    pub id: &'a str,
    /// Where its records are kept on this machine: its file, or the directory its file is
    /// in; `None` in memory or in the host's storage. A sandbox that can read the working
    /// directory must not reach them.
    pub location: Option<&'a Path>,
    /// Variables that commands must not see: the API key's and the other credentials'.
    pub hidden_env: &'a [String],
}

/// The executors every application has: `local`, and `container` with the settings of
/// `[container]`.
pub(super) fn built_in(container: &ContainerSettings) -> Registry<dyn ExecutorFactory> {
    let local: Arc<dyn ExecutorFactory> = Arc::new(Local);
    let container: Arc<dyn ExecutorFactory> = Arc::new(Container(container.clone()));
    Registry::new(
        "executor",
        vec![(LOCAL.to_string(), local), (CONTAINER.to_string(), container)],
    )
}

/// Commands on this machine, as the agent's user, without the hidden variables.
struct Local;

#[async_trait]
impl ExecutorFactory for Local {
    async fn start(&self, session: ExecutorSession<'_>) -> Result<Arc<dyn Executor>> {
        Ok(Arc::new(LocalExecutor {
            hidden_env: session.hidden_env.to_vec(),
        }))
    }
}

/// Commands in a Docker container started for each session and labelled with its ID, with
/// the settings of `[container]`; they see none of the agent's environment.
struct Container(ContainerSettings);

#[async_trait]
impl ExecutorFactory for Container {
    fn isolates(&self) -> bool {
        true
    }

    /// Configuration errors are reported before the container starts.
    async fn start(&self, session: ExecutorSession<'_>) -> Result<Arc<dyn Executor>> {
        // A few synchronous reads of metadata: the location's parts that do not exist yet, and
        // the real path of the rest.
        check_session_outside(session.location, session.cwd)?;
        let config = ContainerConfig {
            label: session.id.into(),
            ..self.0.container_config()?
        };
        let container = ContainerExecutor::start(config, session.cwd)
            .await
            .map_err(in_config_terms)
            .context("cannot start the container")?;
        Ok(Arc::new(container))
    }
}

/// The container reads the whole working directory, so the session file must be outside it:
/// masking one path would leave hard links to it and earlier session files beside it readable.
/// `location` is the session file or the directory it is kept in; `None` in memory.
fn check_session_outside(location: Option<&Path>, cwd: &Path) -> Result<()> {
    if let Some(path) = location {
        anyhow::ensure!(
            !resolve_location(path)?.starts_with(cwd),
            "container execution needs the session file outside the working directory, which the container can read: {}; keep the sessions outside {}",
            path.display(),
            cwd.display()
        );
    }
    Ok(())
}

/// `path` with its deepest existing ancestor resolved, so that a file that does not exist yet
/// is still compared by where it will be created.
fn resolve_location(path: &Path) -> Result<PathBuf> {
    let absolute = std::env::current_dir()
        .context("cannot find the current directory")?
        .join(path);
    let mut existing = absolute.as_path();
    let mut missing = Vec::new();
    while std::fs::symlink_metadata(existing).is_err() {
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name);
                existing = parent;
            }
            _ => break,
        }
    }
    let mut resolved = existing
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", existing.display()))?;
    resolved.extend(missing.iter().rev());
    Ok(resolved)
}

/// `error`, the failure of a container's start, with the advice that names the executor's
/// settings said in the keys of config.toml instead.
fn in_config_terms(error: anyhow::Error) -> anyhow::Error {
    let Some(problem) = error.downcast_ref::<UnusableWorkspace>() else {
        return error;
    };
    let message = match problem {
        UnusableWorkspace::Unreadable { user, workspace } => format!(
            "user {user} cannot read the workspace {}; make it readable by others, or set container.user",
            workspace.display()
        ),
        UnusableWorkspace::Unwritable { user, workspace } => format!(
            "user {user} cannot write the workspace {}; set container.user to its owner, or mount it read-only (container.workspace = \"readOnly\")",
            workspace.display()
        ),
    };
    anyhow::anyhow!(message)
}
