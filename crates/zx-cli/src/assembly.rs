//! An agent made from its configuration: config.toml, the agent file and what one start of
//! the program says over them. Every way in (run, chat, acp, im) builds its agent here, and
//! none of this knows a command line or a terminal.
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

use zx_agent::hooks::{Hook, HookChain, HookOptions};

pub(crate) use agent::{Approvals, ModeAgent};
pub(crate) use place::SessionPlace;
pub(crate) use settings::Settings;
pub(crate) use shared::Shared;

/// What one start of the program says over config.toml and the agent: `None` keeps their
/// value. A command line fills it from its arguments; another way in fills it its own way.
#[derive(Default)]
pub(crate) struct Overrides {
    /// The model ID, over `model` of config.toml.
    pub(crate) model: Option<String>,
    /// Where new sessions work; the current directory without it.
    pub(crate) cwd: Option<PathBuf>,
    /// The enabled tools, in place of the agent's.
    pub(crate) tools: Option<Vec<String>>,
    /// Model requests per run, failed attempts included.
    pub(crate) max_model_requests: Option<u32>,
    /// Wall-clock limit per run, in seconds; 0 removes the agent's.
    pub(crate) timeout_secs: Option<u64>,
    /// Tokens per run; 0 removes the agent's.
    pub(crate) token_budget: Option<u64>,
    /// Spending per run, in the unit of the configured prices; 0 removes the agent's.
    pub(crate) cost_budget: Option<f64>,
}

/// Register a built-in hook under a fixed name, which `hook_failed` events and `[hook]` lines
/// show.
///
/// Without one the name is the hook's type name, whose form Rust does not guarantee across
/// compiler versions. MCP server names cannot contain `_`, so these never clash with an
/// extension's.
fn push_built_in(hooks: &mut HookChain, name: &str, hook: impl Hook + 'static) {
    hooks.push_with(
        hook,
        HookOptions {
            name: Some(name.to_string()),
            ..HookOptions::default()
        },
    );
}

/// The working directory of a new session, which is what its header records: `cwd` made
/// absolute, or the current directory.
pub(crate) fn resolve_cwd(cwd: Option<&Path>) -> Result<PathBuf> {
    let cwd = match cwd {
        Some(path) => path
            .canonicalize()
            .with_context(|| format!("invalid --cwd {}", path.display()))?,
        None => std::env::current_dir().context("cannot find the current directory")?,
    };
    anyhow::ensure!(cwd.is_dir(), "--cwd is not a directory: {}", cwd.display());
    Ok(cwd)
}

/// Refuse `--cwd` when it names another directory than `session_cwd`, where the session to
/// continue works: a session goes on in the directory it started in, and `--cwd` is where
/// new ones work. Without `--cwd` nothing is asked.
pub(crate) fn check_session_cwd(explicit: Option<&Path>, session_cwd: &str) -> Result<()> {
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
        "--cwd {} is not where the session works: it started in {session_cwd} and goes on there; leave --cwd out to continue it",
        asked.display()
    );
    Ok(())
}

/// The value of the environment variable `name`, when it is set and not empty.
pub(crate) fn env_value(name: &str) -> Option<String> {
    // A value that is not UTF-8 holds no usable key, token or path: it counts as unset.
    std::env::var(name).ok().filter(|value| !value.is_empty())
}
