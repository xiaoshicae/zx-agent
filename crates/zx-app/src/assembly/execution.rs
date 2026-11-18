//! Where a session's commands run: this machine, or a Docker container started for the
//! session, with the checks container execution needs and its failures in the terms of
//! config.toml.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::config::{ContainerSettings, Execution};
use zx_agent::builtin::executor::{ContainerConfig, ContainerExecutor, Executor, LocalExecutor, UnusableWorkspace};

/// The container reads the whole working directory, so the session file must be outside it:
/// masking one path would leave hard links to it and earlier session files beside it readable.
/// `location` is the session file or the directory it is kept in; `None` in memory.
pub(super) fn check_session_outside(location: Option<&Path>, cwd: &Path) -> Result<()> {
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

/// Where commands run in `cwd`: this machine, or a container started for the session, which
/// is labelled `label` and takes its settings from `container`; the session file must already
/// be outside `cwd`. On this machine commands do not see the variables `hidden_env` names; in
/// a container they see none of the agent's environment.
pub(super) async fn executor(
    execution: Execution,
    container: &ContainerSettings,
    cwd: &Path,
    label: &str,
    hidden_env: Vec<String>,
) -> Result<Arc<dyn Executor>> {
    match execution {
        Execution::Local => Ok(Arc::new(LocalExecutor { hidden_env })),
        Execution::Container => {
            let config = ContainerConfig {
                label: label.into(),
                ..container.container_config()?
            };
            let container = ContainerExecutor::start(config, cwd)
                .await
                .map_err(in_config_terms)
                .context("cannot start the container for container execution")?;
            Ok(Arc::new(container))
        }
    }
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
