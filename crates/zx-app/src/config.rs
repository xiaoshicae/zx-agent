//! The configuration, outside the agent loop: the zx-agent home directory, the `config.toml`
//! in it, which says where and with what an agent runs, and the agents, which say what an
//! agent does and may do. Loading is synchronous, bounded startup I/O; the assembly
//! (`crate::assembly`) makes agents from what is loaded here.
//!
//! - `environment`: `config.toml`: the model, where commands run, the MCP servers, what is
//!   known about models, and where long tool results are saved
//! - `provider`: its `[provider]` table: which provider reaches the model, and the keys of
//!   the built-in ones
//! - `definition`: an agent: the built-in agents, the user's files that replace them, and the
//!   checks of the result
//! - `agent_file`: an agent file: TOML front matter and instructions, and how it goes over the
//!   built-in agent it extends
//! - `permissions`: an agent's `[permissions]`: the tools that ask, the calls allowed and
//!   the ones denied, in the terms of the policies
//! - `context`: the project context: `AGENTS.md` files from the Git root down to the working
//!   directory
//! - `skills`: an agent's skills: their catalog, whose bodies a tool reads on demand

mod agent_file;
mod context;
mod definition;
mod environment;
mod permissions;
mod provider;
mod skills;

use std::path::{Path, PathBuf};

use anyhow::{Result, ensure};

pub(crate) use agent_file::{ModelRequestLimit, RawLimits, cost_budget};
pub(crate) use context::ProjectContext;
pub use definition::Builtin;
pub(crate) use definition::{AgentDefinition, HostTools, known_tool};
pub use environment::{CONTAINER, Config, ContainerSettings, LOCAL, McpServer};
pub(crate) use permissions::Permissions;
pub use provider::{ANTHROPIC, OPENAI, ProviderSettings};
pub(crate) use provider::{anthropic_config, openai_config};
pub(crate) use skills::{READ_SKILL, Skill};

use zx_agent::builtin::files::read_text_file;
use zx_util::env::env_value;

/// The variable that names the zx-agent home directory instead of `~/.zx-agent`.
pub const HOME_ENV: &str = "ZX_AGENT_HOME";

/// Under the user's home directory: the zx-agent home directory, unless [`HOME_ENV`] names
/// another.
const HOME_DIR: &str = ".zx-agent";

/// Maximum size of an agent file, context file or skill body.
pub(crate) const MAX_FILE_BYTES: u64 = 64 * 1024;

/// The zx-agent home directory, which holds `config.toml`, `agents/` and `sessions/`:
/// [`HOME_ENV`] when it is set and not empty, else `~/.zx-agent`; `None` when neither is
/// known. A relative [`HOME_ENV`] is an error: it would depend on the working directory. A
/// value that is not UTF-8 counts as unset, as for every variable the configuration reads.
pub fn home() -> Result<Option<PathBuf>> {
    match env_value(HOME_ENV) {
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

/// Read a configuration file the way config.toml is read: a regular UTF-8 file of at most
/// 64 KiB, the limit of agent files too.
///
/// Synchronous: meant for startup, before anything runs concurrently. Fails when the file
/// cannot be read, is not a regular file, is larger or is not UTF-8; the error names the
/// path.
pub fn read_text(path: &Path) -> Result<String> {
    read_text_file(path, MAX_FILE_BYTES)
}

/// `value` parsed by its type, with `key` naming it in the error.
fn parsed<T: std::str::FromStr<Err = String>>(key: &str, value: Option<String>) -> Result<Option<T>> {
    value
        .map(|text| text.parse().map_err(|error: String| anyhow::anyhow!("{key}: {error}")))
        .transpose()
}
