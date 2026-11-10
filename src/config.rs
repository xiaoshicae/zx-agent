//! The configuration of the command, outside the agent loop: the zx-agent home directory,
//! the `config.toml` in it, which says where and with what an agent runs, and the agents,
//! which say what an agent does and may do. Loading is synchronous, bounded startup I/O; tool
//! and frontend assembly remains the caller's responsibility.
//!
//! - `environment`: `config.toml`: the model and its provider, where commands run, the MCP
//!   servers, what is known about models, and where long tool results are saved
//! - `definition`: an agent: the built-in agents, the user's files that replace them, and the
//!   checks of the result
//! - `agent_file`: an agent file: TOML front matter and instructions, and how it goes over the
//!   built-in agent it extends
//! - `permissions`: an agent's `[permissions]`: the tools that ask, the calls allowed and
//!   the ones denied, in the terms of the policies
//! - `context`: the project context: `AGENTS.md` files from the Git root down to the working
//!   directory
//! - `skills`: explicit skill catalogs, whose bodies a tool reads on demand

mod agent_file;
mod context;
mod definition;
mod environment;
mod permissions;
pub mod skills;

use std::path::{Path, PathBuf};

use anyhow::{Result, ensure};

pub use definition::{AgentDefinition, Builtin, ProjectContext};
pub use environment::{Config, ContainerSettings, Execution, McpServer, ProviderSettings};
pub use permissions::{Entry, Permissions};

use crate::files::read_text_file;

/// The variable that names the zx-agent home directory instead of `~/.zx-agent`.
pub const HOME_ENV: &str = "ZX_AGENT_HOME";

/// Under the user's home directory: the zx-agent home directory, unless [`HOME_ENV`] names
/// another.
const HOME_DIR: &str = ".zx-agent";

/// Maximum size of an agent file, context file or skill body.
const MAX_FILE_BYTES: u64 = 64 * 1024;

/// The zx-agent home directory, which holds `config.toml`, `agents/` and `sessions/`:
/// [`HOME_ENV`] when it is set and not empty, else `~/.zx-agent`; `None` when neither is
/// known. A relative [`HOME_ENV`] is an error: it would depend on the working directory.
pub fn home() -> Result<Option<PathBuf>> {
    match std::env::var_os(HOME_ENV).filter(|value| !value.is_empty()) {
        Some(value) => {
            let path = PathBuf::from(value);
            ensure!(
                path.is_absolute(),
                "{HOME_ENV} must be an absolute path, not {}",
                path.display()
            );
            Ok(Some(path))
        }
        None => Ok(std::env::home_dir().map(|home| home.join(HOME_DIR))),
    }
}

/// Read a regular UTF-8 configuration file within [`MAX_FILE_BYTES`]. This is bounded,
/// synchronous startup I/O; invalid files and read failures preserve their path in errors.
fn read_text(path: &Path) -> Result<String> {
    read_text_file(path, MAX_FILE_BYTES)
}
