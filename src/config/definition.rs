//! An agent: what it does and may do. Each mode runs a built-in agent (`run` the unattended
//! one; `chat`, `serve` and `acp` the assistant), which a file of the same name in the
//! `agents/` directory of the zx-agent home replaces.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

use super::agent_file::AgentFile;
use super::skills::{self, Skill};
use super::{Config, Execution, context, read_text};
use crate::policy::{AutoApprove, ProgramOptions};
use crate::runner::Limits;
use crate::tools::BUILTIN_TOOL_NAMES;
use crate::{compaction, output_store};

/// Under the zx-agent home: the user's agents, which replace the built-in ones of their names.
const AGENTS_DIR: &str = "agents";

/// A built-in agent; an agent file may extend one of these, never another file.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Builtin {
    /// A neutral agent without a human operator, the default of `run`.
    Unattended,
    /// A conversation with a person, the default of `chat`, `serve` and `acp`.
    Assistant,
}

impl Builtin {
    /// Every built-in agent.
    pub const ALL: [Builtin; 2] = [Self::Unattended, Self::Assistant];

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

/// Host-enforced command and tool rules.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct Rules {
    /// Bash command prefixes; an empty list leaves the original deny-only policy.
    pub allow: Vec<String>,
    /// Additional case-insensitive command substrings to deny.
    pub deny: Vec<String>,
    /// Bash command prefixes that run without asking where bash needs approval; the commands
    /// that still ask are listed on [`crate::policy::AutoApprove`].
    pub auto_approve: Vec<String>,
    /// Texts that keep a command asking although `auto_approve` covers it, compared without
    /// case, such as `secret`.
    pub auto_approve_except: Vec<String>,
    /// For a program, such as kubectl, the options that a covered command may pass; any other
    /// option asks. Programs not listed may pass any option. A file replaces the entry of a
    /// program it lists.
    pub auto_approve_options: BTreeMap<String, ProgramOptions>,
    /// Registered tool names whose calls require a host approval.
    pub ask_tools: Vec<String>,
    /// Tool names denied regardless of enabled tools or approval grants.
    pub deny_tools: Vec<String>,
    /// Ask nobody: calls that would wait for an approval run instead, while deny rules and
    /// the command policy still apply. Only with container execution, where the container is
    /// the sandbox, and not together with `ask_tools`.
    pub approve_all: bool,
}

impl Rules {
    /// The bash commands these rules let run without asking, for the approval rules.
    pub fn auto_approval(&self) -> AutoApprove {
        self.auto_approve_options.iter().fold(
            AutoApprove::new(&self.auto_approve).except(&self.auto_approve_except),
            |list, (program, options)| list.options(program, options.clone()),
        )
    }
}

/// Which project instructions to read at startup. Explicit paths are absolute after load.
#[derive(Debug)]
pub struct ProjectContext {
    /// Discover root-to-cwd AGENTS.md files, bounded by the nearest Git root.
    pub discover: bool,
    /// Extra instruction files, resolved relative to the agent file at load time.
    pub files: Vec<PathBuf>,
}

/// A resolved agent: its file over the built-in agent it extends. Command-line overrides
/// are applied by the frontend, which calls `validate` again before assembly.
#[derive(Debug)]
pub struct AgentDefinition {
    /// The file stem, or the built-in agent's name.
    pub name: String,
    /// Effective preamble, before generated machine and project sections.
    pub instructions: String,
    /// Requested built-in tools; read_skill is omitted when no catalog is registered.
    pub tools: Vec<String>,
    /// Rules enforced by the host, independently of prompt text.
    pub rules: Rules,
    /// Per-prompt request and time budgets.
    pub limits: Limits,
    /// Number of length-truncated responses to continue automatically.
    pub continue_on_length: u32,
    /// Project guidance discovery and explicit file paths.
    pub context: ProjectContext,
    /// Catalog metadata; loading does not decode the skill bodies.
    pub skills: Vec<Skill>,
    /// Automatic trigger and bounded manual summarization settings.
    pub compaction: compaction::Config,
}

impl AgentDefinition {
    /// The agent `kind`, checked against `config`: the user's `agents/<name>.md` in the
    /// zx-agent `home` when that file exists, else the built-in agent. No other file is read:
    /// a file in `agents/` with another name is ignored.
    pub fn load(kind: Builtin, home: Option<&Path>, config: &Config) -> Result<Self> {
        let agent = match home.map(|home| home.join(AGENTS_DIR).join(format!("{}.md", kind.name()))) {
            Some(path) if exists(&path)? => Self::from_file(&path)?,
            _ => Self::builtin(kind)?,
        };
        agent.validate(config)?;
        Ok(agent)
    }

    /// The agent of the file at `path`, over the built-in agent it extends, named after the
    /// file.
    fn from_file(path: &Path) -> Result<Self> {
        let path = path
            .canonicalize()
            .with_context(|| format!("cannot locate agent {}", path.display()))?;
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .with_context(|| format!("agent {} has no name", path.display()))?
            .to_string();
        let file = AgentFile::parse(&read_text(&path)?).with_context(|| format!("invalid agent {}", path.display()))?;
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
        file.apply(&mut agent, parent)
            .with_context(|| format!("invalid agent {}", path.display()))?;
        agent.name = name;
        Ok(agent)
    }

    /// The built-in agent `kind`, over the empty base.
    fn builtin(kind: Builtin) -> Result<Self> {
        let file = AgentFile::parse(kind.source()).context("invalid built-in agent")?;
        ensure!(file.extends().is_none(), "a built-in agent extends another");
        let mut agent = Self::empty();
        file.apply(&mut agent, Path::new("."))?;
        agent.name = kind.name().to_string();
        Ok(agent)
    }

    /// What an agent file without `extends` goes over: no tools, rules, skills or
    /// instructions, the kernel's limits and summary settings, and no project context.
    fn empty() -> Self {
        Self {
            name: String::new(),
            instructions: String::new(),
            tools: Vec::new(),
            rules: Rules::default(),
            limits: Limits::default(),
            continue_on_length: 0,
            context: ProjectContext {
                discover: false,
                files: Vec::new(),
            },
            skills: Vec::new(),
            compaction: compaction::Config::default(),
        }
    }

    /// Check names, rule patterns and skill registrations without reading skill bodies; tool
    /// rules may name the MCP tools of `config`, and approving every call needs its container.
    pub fn validate(&self, config: &Config) -> Result<()> {
        for name in &self.tools {
            ensure!(known_tool(name), "unknown tool {name:?}");
        }
        for pattern in self
            .rules
            .allow
            .iter()
            .chain(&self.rules.deny)
            .chain(&self.rules.auto_approve)
            .chain(&self.rules.auto_approve_except)
        {
            ensure!(!pattern.trim().is_empty(), "empty command rule is not allowed");
        }
        for (program, options) in &self.rules.auto_approve_options {
            check_program_options(program, options)?;
        }
        let mcp_tools = config.mcp_tool_names()?;
        for tool in self.rules.ask_tools.iter().chain(&self.rules.deny_tools) {
            ensure!(
                known_tool(tool) || mcp_tools.contains(tool),
                "unknown tool in rule {tool:?}"
            );
        }
        self.compaction.validate()?;
        if self.rules.approve_all {
            ensure!(
                config.execution == Execution::Container,
                "rules.approveAll needs execution = \"container\": without the container as a sandbox, every command would run on this machine unasked"
            );
            ensure!(
                self.rules.ask_tools.is_empty(),
                "rules.approveAll asks for no approval, so rules.askTools must be empty"
            );
        }
        skills::validate(&self.skills)
    }

    /// Read root-to-cwd project guidance and explicit files, within a total byte budget.
    pub fn context_section(&self, cwd: &Path) -> Result<String> {
        context::load(cwd, &self.context)
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

/// Whether `name` is a tool the host registers itself: a built-in, read_skill or read_output.
fn known_tool(name: &str) -> bool {
    name == skills::TOOL_NAME || name == output_store::TOOL_NAME || BUILTIN_TOOL_NAMES.contains(&name)
}

/// A program name of `rules.autoApproveOptions` is one lowercase word, since commands are
/// compared without case; options are written without their dashes, and a letter is either
/// a flag or takes a value.
fn check_program_options(program: &str, options: &ProgramOptions) -> Result<()> {
    ensure!(
        !program.is_empty() && !program.contains(char::is_whitespace) && program.to_lowercase() == program,
        "autoApproveOptions: program {program:?} must be one lowercase word"
    );
    for name in &options.long {
        ensure!(
            !name.is_empty() && !name.starts_with('-') && !name.contains(|c: char| c.is_whitespace() || c == '='),
            "autoApproveOptions.{program}: long option {name:?} must be a name without dashes, such as \"namespace\""
        );
    }
    for letter in options.short_with_value.iter().chain(&options.short_flags) {
        ensure!(
            letter.is_ascii_alphanumeric(),
            "autoApproveOptions.{program}: short option {letter:?} must be a letter or digit"
        );
    }
    if let Some(letter) = options
        .short_flags
        .iter()
        .find(|letter| options.short_with_value.contains(letter))
    {
        bail!("autoApproveOptions.{program}: {letter:?} cannot be both a flag and an option with a value");
    }
    Ok(())
}

#[cfg(test)]
mod tests;
