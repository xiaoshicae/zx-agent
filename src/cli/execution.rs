//! Where a session's commands run: this machine, or a Docker container started for the
//! session, with the checks container execution needs.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};

use zx_agent::executor::{ContainerConfig, ContainerExecutor, Executor, LocalExecutor, container_environment_section};
use zx_agent::profile::{ContainerSettings, Execution, Profile};
use zx_agent::prompt::environment_section;

use crate::session_files::SessionFile;

/// The container reads the whole working directory, so the session file must be outside it:
/// masking one path would leave hard links to it and earlier session files beside it readable.
pub(super) fn check_session_outside(file: &SessionFile<'_>, cwd: &Path) -> Result<()> {
    if let Some(path) = file.location() {
        anyhow::ensure!(
            !resolve_location(path)?.starts_with(cwd),
            "container execution needs the session file outside the working directory, which the container can read: {}; choose a --session path (in chat also --session-dir) outside {}",
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

/// Where commands run, started when it is a container, and the prompt section describing it.
/// On this machine commands do not see the variables `hidden_env` names; in a container they
/// see none of the agent's environment.
pub(super) async fn executor(
    profile: &Profile,
    cwd: &Path,
    label: &str,
    hidden_env: Vec<String>,
) -> Result<(Arc<dyn Executor>, String)> {
    match profile.execution {
        Execution::Local => {
            let local: Arc<dyn Executor> = Arc::new(LocalExecutor { hidden_env });
            Ok((local, environment_section(cwd)))
        }
        Execution::Container => {
            let config = container_config(&profile.container, cwd, label)?;
            // The description needs the settings, which starting the container consumes.
            let settings = config.clone();
            let container = ContainerExecutor::start(config)
                .await
                .context("cannot start the container for container execution")?;
            let environment = container_environment_section(&settings, container.platform());
            let container: Arc<dyn Executor> = Arc::new(container);
            Ok((container, environment))
        }
    }
}

/// Container settings from the profile; the session file must already be outside `cwd`.
fn container_config(settings: &ContainerSettings, cwd: &Path, label: &str) -> Result<ContainerConfig> {
    // `load_profile` has checked both fields; the contexts only name them.
    let image = settings.image.clone().context("container.image")?;
    let mut config = ContainerConfig {
        label: label.into(),
        ..ContainerConfig::new(image, cwd)
    };
    if let Some(user) = &settings.user {
        config.user.clone_from(user);
    }
    if let Some(memory) = &settings.memory {
        config.memory.clone_from(memory);
    }
    if let Some(limit) = settings.pids_limit {
        config.pids_limit = limit;
    }
    if let Some(network) = &settings.network {
        config.network = network.parse().context("container.network")?;
    }
    if let Some(access) = settings.workspace {
        config.workspace_access = access;
    }
    if let Some(access) = settings.root {
        config.root_access = access;
    }
    Ok(config)
}
