//! File-based scenario configuration, outside the agent loop. Loading is synchronous,
//! bounded startup I/O; tool and frontend assembly remains the caller's responsibility.
//!
//! - `file`: the TOML layer of a profile, and how one layer goes over another
//! - `context`: the project context: `AGENTS.md` files from the Git root down to the working
//!   directory
//! - `skills`: explicit skill catalogs, whose bodies a tool reads on demand

mod context;
mod file;
pub mod skills;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

use file::RawProfile;
use file::read_text;
use skills::Skill;

use crate::config::{Config, Execution};
use crate::policy::{AutoApprove, ProgramOptions};
use crate::runner::Limits;
use crate::tools::BUILTIN_TOOL_NAMES;
use crate::{compaction, output_store};

/// Maximum size of a profile, instructions, context file or skill body.
const MAX_FILE_BYTES: u64 = 64 * 1024;

/// Embedded baseline; custom profiles may extend one of these, never arbitrary files.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Builtin {
    /// A neutral agent without a human operator, the default of `run`.
    Unattended,
    /// A conversation with a person, the default of `chat` and `serve`.
    Assistant,
}

/// An embedded profile's files.
struct Source {
    toml: &'static str,
    /// What the TOML names as its instructions, which must be `instructions`.
    instructions_file: &'static str,
    instructions: &'static str,
}

impl Builtin {
    /// Every embedded profile.
    const ALL: [Builtin; 2] = [Self::Unattended, Self::Assistant];

    /// The name `--profile` and `extends` select it by.
    pub fn name(self) -> &'static str {
        match self {
            Self::Unattended => "unattended",
            Self::Assistant => "assistant",
        }
    }

    fn source(self) -> Source {
        match self {
            Self::Unattended => Source {
                toml: include_str!("../profiles/unattended.toml"),
                instructions_file: "unattended.md",
                instructions: include_str!("../profiles/unattended.md"),
            },
            Self::Assistant => Source {
                toml: include_str!("../profiles/assistant.toml"),
                instructions_file: "assistant.md",
                instructions: include_str!("../profiles/assistant.md"),
            },
        }
    }
}

/// Host-enforced command and tool rules. Denials are accumulated across layers.
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
    /// case, such as `secret`. Accumulated across layers, like `deny`.
    pub auto_approve_except: Vec<String>,
    /// For a program, such as kubectl, the options that a covered command may pass; any other
    /// option asks. Programs not listed may pass any option. A layer replaces the entry of a
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
    /// Extra instruction files, resolved relative to the profile file at load time.
    pub files: Vec<PathBuf>,
}

/// Fully layered scenario configuration. CLI overrides are applied by the frontend;
/// callers must keep deny rules additive and call `validate` before assembly.
#[derive(Debug)]
pub struct Profile {
    /// Human-readable scenario name.
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

impl Profile {
    /// Load an embedded name or a TOML file over the selected default, checked against the
    /// environment `config`. Relative profile paths use the process working directory; paths
    /// inside TOML use its parent. Unknown fields and invalid files are errors.
    pub fn load(selector: Option<&str>, default: Builtin, config: &Config) -> Result<Self> {
        let Some(selector) = selector else {
            let profile = Self::builtin(default)?;
            profile.validate(config)?;
            return Ok(profile);
        };
        if let Some(builtin) = Builtin::ALL.into_iter().find(|builtin| builtin.name() == selector) {
            let profile = Self::builtin(builtin)?;
            profile.validate(config)?;
            return Ok(profile);
        }
        ensure!(!selector.trim().is_empty(), "profile selector is empty");
        let path = Path::new(selector)
            .canonicalize()
            .with_context(|| format!("cannot locate profile {selector:?}"))?;
        let raw: RawProfile =
            toml::from_str(&read_text(&path)?).with_context(|| format!("invalid profile {}", path.display()))?;
        let mut profile = Self::builtin(raw.extends.unwrap_or(default))?;
        // A canonical file path always has a parent, but keep this error actionable.
        let parent = path.parent().context("profile has no parent directory")?;
        raw.apply(&mut profile, parent)?;
        profile.validate(config)?;
        Ok(profile)
    }

    fn builtin(kind: Builtin) -> Result<Self> {
        let source = kind.source();
        let mut raw: RawProfile = toml::from_str(source.toml).context("invalid embedded profile")?;
        ensure!(
            raw.instructions.take().as_deref() == Some(Path::new(source.instructions_file)),
            "embedded instructions path is inconsistent"
        );
        let mut profile = Self {
            name: String::new(),
            instructions: source.instructions.into(),
            tools: Vec::new(),
            rules: Rules::default(),
            limits: Limits::default(),
            continue_on_length: 0,
            context: ProjectContext {
                discover: true,
                files: Vec::new(),
            },
            skills: Vec::new(),
            compaction: compaction::Config::default(),
        };
        raw.apply(&mut profile, Path::new("."))?;
        profile.validate(&Config::default())?;
        Ok(profile)
    }

    /// Check names, rule patterns and skill registrations without reading skill bodies; tool
    /// rules may name the MCP tools of `config`, and approving every call needs its container.
    pub fn validate(&self, config: &Config) -> Result<()> {
        ensure!(!self.name.trim().is_empty(), "profile name is empty");
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
