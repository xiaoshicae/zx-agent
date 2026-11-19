//! An agent: what it does and may do. A host runs one of the built-in agents, which a file of
//! the same name in the `agents/` directory of the zx-agent home replaces.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

use super::agent_file::{AgentFile, compaction_error, settle_token_trigger};
use super::permissions::Permissions;
use super::skills::{READ_SKILL, Skill, validate_skills};
use super::{Config, context, read_text};
use zx_agent::agent::RunLimits;
use zx_agent::builtin::tools::{EXECUTOR_TOOL_NAMES, READ_OUTPUT};
use zx_agent::compaction::{self, DEFAULT_RESERVE_TOKENS, TokenTrigger};

/// Under the zx-agent home: the user's agents, which replace the built-in ones of their names.
const AGENTS_DIR: &str = "agents";

/// A built-in agent: the one a host runs, unless the user's file of its name replaces it. An
/// agent file may extend one of these, never another file.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Builtin {
    /// A neutral agent for runs that nobody attends: approvals have no one to answer them.
    Unattended,
    /// An agent for a conversation with a person, who answers its approvals.
    Assistant,
}

impl Builtin {
    /// Its name, as `extends` and the user's file in `agents/` spell it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Unattended => "unattended",
            Self::Assistant => "assistant",
        }
    }

    /// The agent file embedded in the program.
    fn source(self) -> &'static str {
        match self {
            Self::Unattended => include_str!("../../agents/unattended.md"),
            Self::Assistant => include_str!("../../agents/assistant.md"),
        }
    }
}

/// The tools a host registers besides the built-in ones, by name, which an agent may name:
/// `own`, which the agent's `tools` enables and which run unless `permissions.ask` names them,
/// as the built-in tools do; and those of its `extensions`, offered to every agent, which ask
/// unless `permissions.allow` names them, as the tools of MCP servers do.
#[derive(Default)]
pub(crate) struct HostTools {
    pub(crate) own: Vec<String>,
    pub(crate) extensions: Vec<String>,
}

/// Which project instructions a session reads when it is set up. Explicit paths are
/// absolute after load.
#[derive(Debug)]
pub(crate) struct ProjectContext {
    /// Discover root-to-cwd AGENTS.md files, bounded by the nearest Git root.
    pub(crate) discover: bool,
    /// Extra instruction files, resolved relative to the agent file at load time.
    pub(crate) files: Vec<PathBuf>,
}

impl ProjectContext {
    /// Read root-to-cwd project guidance and explicit files, within a total byte budget.
    pub(crate) fn section(&self, cwd: &Path) -> Result<String> {
        context::load(cwd, self)
    }
}

/// A resolved agent: its file over the built-in agent it extends. The assembly lays what a
/// host says over the agent ([`Overrides`](crate::Overrides)) on it and checks it again.
#[derive(Debug)]
pub(crate) struct AgentDefinition {
    /// The file stem, or the built-in agent's name.
    pub(crate) name: String,
    /// Effective preamble, before generated machine and project sections.
    pub(crate) instructions: String,
    /// Requested built-in tools; read_skill is omitted when no catalog is registered.
    pub(crate) tools: Vec<String>,
    /// What the host lets the agent do, independently of prompt text.
    pub(crate) permissions: Permissions,
    /// Per-prompt request and time budgets.
    pub(crate) limits: RunLimits,
    /// Number of length-truncated responses to continue automatically.
    pub(crate) continue_on_length: u32,
    /// Project guidance discovery and explicit file paths.
    pub(crate) context: ProjectContext,
    /// Catalog metadata; loading does not decode the skill bodies.
    pub(crate) skills: Vec<Skill>,
    /// Automatic trigger and bounded manual summarization settings. A token trigger that no
    /// file sets follows the context window of the model in use while the byte trigger is on.
    pub(crate) compaction: compaction::Config,
}

impl AgentDefinition {
    /// The agent `builtin`, checked against `config` and the tools of the `host`: the user's
    /// `agents/<name>.md` in the zx-agent `home` when that file exists, else the built-in
    /// agent. No other file is read: a file in `agents/` with another name is ignored.
    pub(crate) fn load(builtin: Builtin, home: Option<&Path>, config: &Config, host: &HostTools) -> Result<Self> {
        match home.map(|home| home.join(AGENTS_DIR).join(format!("{}.md", builtin.name()))) {
            Some(path) if exists(&path)? => Self::from_file(&path, config, host),
            _ => Self::builtin(builtin)?.checked(config, host),
        }
    }

    /// The agent of the file at `path`, over the built-in agent it extends, named after the
    /// file and [`checked`](Self::checked); an error in the file names it.
    fn from_file(path: &Path, config: &Config, host: &HostTools) -> Result<Self> {
        let path = path
            .canonicalize()
            .with_context(|| format!("cannot locate agent {}", path.display()))?;
        let invalid = || format!("invalid agent {}", path.display());
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .with_context(|| format!("agent {} has no name", path.display()))?
            .to_string();
        let file = AgentFile::parse(&read_text(&path)?).with_context(invalid)?;
        let mut agent = match file.extends() {
            Some(builtin) => Self::builtin(builtin)?,
            None => {
                ensure!(
                    file.has_instructions(),
                    "agent {} has no instructions: write them below the front matter or extend a built-in agent",
                    path.display()
                );
                Self::empty()
            }
        };
        // A canonical file path always has a parent, but keep this error actionable.
        let parent = path.parent().context("the agent has no parent directory")?;
        file.apply(&mut agent, parent).with_context(invalid)?;
        agent.name = name;
        agent.checked(config, host).with_context(invalid)
    }

    /// The agent once every file of it is applied: its token trigger settled
    /// ([`settle_token_trigger`]) and its settings checked against `config` and the tools of
    /// the `host`.
    fn checked(mut self, config: &Config, host: &HostTools) -> Result<Self> {
        settle_token_trigger(&mut self.compaction);
        self.validate(config, host)?;
        Ok(self)
    }

    /// The built-in agent `builtin`, over the empty base.
    fn builtin(builtin: Builtin) -> Result<Self> {
        let file = AgentFile::parse(builtin.source()).context("invalid built-in agent")?;
        ensure!(file.extends().is_none(), "a built-in agent extends another");
        let mut agent = Self::empty();
        file.apply(&mut agent, Path::new("."))?;
        agent.name = builtin.name().to_string();
        Ok(agent)
    }

    /// What an agent file without `extends` goes over: no tools, permissions, skills or
    /// instructions, the kernel's limits and summary settings, and no project context.
    fn empty() -> Self {
        Self {
            name: String::new(),
            instructions: String::new(),
            tools: Vec::new(),
            permissions: Permissions::default(),
            limits: RunLimits::default(),
            continue_on_length: 0,
            context: ProjectContext {
                discover: false,
                files: Vec::new(),
            },
            skills: Vec::new(),
            // Until a file sets `triggerTokens`, the token trigger follows the context window;
            // `load` settles it once every file is applied.
            compaction: compaction::Config {
                trigger_tokens: Some(TokenTrigger::BelowWindow {
                    reserve_tokens: DEFAULT_RESERVE_TOKENS,
                }),
                ..compaction::Config::default()
            },
        }
    }

    /// Check tool names, permissions and skill registrations without reading skill bodies;
    /// the tools may be the `host`'s own, and permissions may name those and the tools of its
    /// extensions and the MCP tools of `config`. Whether commands run where every call may be
    /// approved is the executor's to say, which the assembly checks.
    pub(crate) fn validate(&self, config: &Config, host: &HostTools) -> Result<()> {
        let known = |name: &str| known_tool(name) || host.own.iter().any(|own| own == name);
        for name in &self.tools {
            ensure!(known(name), "tools: unknown tool {name:?}");
        }
        let mut asking = config.mcp_tool_names()?;
        asking.extend(host.extensions.iter().cloned());
        self.permissions.validate(known, &asking)?;
        self.compaction.validate().map_err(|error| compaction_error(&error))?;
        ensure!(
            !self.permissions.approve_all || self.permissions.ask.is_empty(),
            "permissions.approveAll asks for no approval, so permissions.ask must be empty"
        );
        validate_skills(&self.skills)
    }
}

/// Whether `path` exists, as a file, a link or anything else; an error other than its absence
/// stops startup rather than silently leaving a user's agent out.
fn exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("cannot inspect {}", path.display())),
    }
}

/// Whether `name` is one of the built-in tools an agent may enable.
pub(crate) fn known_tool(name: &str) -> bool {
    EXECUTOR_TOOL_NAMES.contains(&name) || name == READ_OUTPUT || name == READ_SKILL
}

#[cfg(test)]
mod tests;
