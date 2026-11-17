//! An agent made from its configuration: config.toml, the agent file and what one start of
//! the program says over them. Every way in (the command line's run, chat, acp and im, and
//! other hosts) builds its agent here, and none of this knows a command line or a terminal.
//!
//! - `settings`: config.toml with the model and two variables over it
//! - `shared`: what every session of a process shares, read or connected once
//! - `agent`: the agent of a mode, and the sessions a mode creates or resumes with it
//! - `setup`: what each session gets for itself: its executor, the tools on it, its store of
//!   long tool results and the prompt sections about its directory
//! - `execution`: where a session's commands run: this machine or a Docker container
//! - `place`: where a mode keeps its sessions: memory, one file, or the home directory

mod agent;
mod execution;
mod place;
mod settings;
mod setup;
mod shared;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub use agent::{Approvals, ConfiguredAgent};
pub use place::{PlaceKind, SessionPlace};
pub use settings::NoModel;
pub(crate) use settings::Settings;
pub(crate) use shared::{Shared, checked_secret_files};

/// What one start of the program says over config.toml and the agent: `None` keeps their
/// value. A command line fills it from its arguments; another way in fills it its own way.
#[derive(Default)]
pub struct Overrides {
    /// The model ID, over `model` of config.toml.
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
    /// Spending per run, in the unit of the configured prices; 0 removes the agent's.
    pub cost_budget: Option<f64>,
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

/// Refuse an explicit working directory ([`Overrides::cwd`]) that is another than
/// `session_cwd`, where the session to continue works: a session goes on in the directory
/// it started in, and the explicit one is where new ones work. Without one nothing is asked.
fn check_session_cwd(explicit: Option<&Path>, session_cwd: &str) -> Result<()> {
    let Some(explicit) = explicit else {
        return Ok(());
    };
    let asked = resolve_cwd(Some(explicit))?;
    // A recorded directory that is gone is not the one asked for either; opening the
    // session says that it is gone.
    let same = Path::new(session_cwd)
        .canonicalize()
        .is_ok_and(|recorded| recorded == asked);
    anyhow::ensure!(
        same,
        "the working directory {} is not where the session works: it started in {session_cwd} and goes on there; give none to continue it",
        asked.display()
    );
    Ok(())
}

/// The value of the environment variable `name`, when it is set and not empty.
pub fn env_value(name: &str) -> Option<String> {
    // A value that is not UTF-8 holds no usable key, token or path: it counts as unset.
    std::env::var(name).ok().filter(|value| !value.is_empty())
}
