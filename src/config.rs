//! The configuration of the command: the zx-agent home directory and the `config.toml` in
//! it, which says where and with what an agent runs. Loading is synchronous, bounded startup
//! I/O.
//!
//! - `environment`: `config.toml`: the model and its provider, where commands run, the MCP
//!   servers, what is known about models, and where long tool results are saved

mod environment;

use std::path::PathBuf;

use anyhow::{Result, ensure};

pub use environment::{Config, ContainerSettings, Execution, McpServer, ProviderSettings};

/// The variable that names the zx-agent home directory instead of `~/.zx-agent`.
pub const HOME_ENV: &str = "ZX_AGENT_HOME";

/// Under the user's home directory: the zx-agent home directory, unless [`HOME_ENV`] names
/// another.
const HOME_DIR: &str = ".zx-agent";

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
