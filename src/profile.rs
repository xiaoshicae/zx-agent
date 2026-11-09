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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

use file::RawProfile;
use file::read_text;
use skills::Skill;

use crate::ai::ModelMeta;
use crate::executor::{Access, check_network};
use crate::mcp::{check_server_name, tool_name as mcp_tool_name};
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

/// An external MCP server whose listed tools the model may call. The frontends connect it at
/// startup; whether a call runs is up to the rules.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct McpServer {
    /// ASCII lowercase letters, digits and `-`. Its tools reach the model as `<name>__<tool>`.
    pub name: String,
    /// The server's MCP endpoint (Streamable HTTP).
    pub url: String,
    /// The variable holding the token sent as `Authorization: Bearer`; unset sends none.
    /// Commands do not see the variable, and its value is redacted from tool output.
    #[serde(default)]
    pub token_env: Option<String>,
    /// The server's tools to offer the model, by the names the server gives them.
    pub tools: Vec<String>,
    /// Tools among `tools` that run without approval. The others ask a person, and are
    /// refused where no one can answer, as in `run`.
    #[serde(default)]
    pub allow_tools: Vec<String>,
    /// Longest a call may go without an answer or a progress notification, in seconds;
    /// unset uses `--tool-timeout-secs`.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Bound for a whole call, in seconds, however much progress the server reports; unset
    /// leaves only `timeoutSecs`, the run's timeout and cancellation.
    #[serde(default)]
    pub max_timeout_secs: Option<u64>,
}

/// Which project instructions to read at startup. Explicit paths are absolute after load.
#[derive(Debug)]
pub struct ProjectContext {
    /// Discover root-to-cwd AGENTS.md files, bounded by the nearest Git root.
    pub discover: bool,
    /// Extra instruction files, resolved relative to the profile file at load time.
    pub files: Vec<PathBuf>,
}

/// Where bash and read_file run.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Execution {
    /// On this machine, as the agent's user.
    #[default]
    Local,
    /// In an isolated Docker container; see [`ContainerSettings`].
    Container,
}

/// Container settings for `Execution::Container`; unset values use the executor defaults.
/// A profile's `[container]` section, whose values replace those of the profile it extends.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct ContainerSettings {
    /// Required in container mode; the image must already be present locally.
    pub image: Option<String>,
    /// `uid:gid` that commands run as.
    pub user: Option<String>,
    /// Memory limit in Docker's syntax, such as `1g`.
    pub memory: Option<String>,
    /// Maximum number of processes in the container.
    pub pids_limit: Option<u32>,
    /// Docker network, such as `bridge` (the executor's default) or `none`.
    pub network: Option<String>,
    /// Whether commands may change the workspace; writable by default.
    pub workspace: Option<Access>,
    /// Whether commands may change the container's own files; writable by default.
    pub root: Option<Access>,
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
    /// Already bounded tool results may be persisted when read_output is enabled.
    pub tool_output: output_store::Config,
    /// Where bash and read_file run.
    pub execution: Execution,
    /// Used when `execution` is `Container`.
    pub container: ContainerSettings,
    /// External MCP servers; a profile's `[[mcp]]` list replaces the one it extends.
    pub mcp: Vec<McpServer>,
    /// What is known about models, by ID (`[[models]]`): the context window and prices. A
    /// profile's entries override those of the profile it extends, by ID.
    pub models: HashMap<String, ModelMeta>,
}

impl Profile {
    /// Load an embedded name or a TOML file over the selected default. Relative profile
    /// paths use the process working directory; paths inside TOML use its parent.
    /// Unknown fields, unsupported backends and invalid files are errors.
    pub fn load(selector: Option<&str>, default: Builtin) -> Result<Self> {
        let Some(selector) = selector else {
            return Self::builtin(default);
        };
        if let Some(builtin) = Builtin::ALL.into_iter().find(|builtin| builtin.name() == selector) {
            return Self::builtin(builtin);
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
        profile.validate()?;
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
            models: HashMap::new(),
            tool_output: output_store::Config::default(),
            execution: Execution::Local,
            container: ContainerSettings::default(),
            mcp: Vec::new(),
        };
        raw.apply(&mut profile, Path::new("."))?;
        profile.validate()?;
        Ok(profile)
    }

    /// The names the model calls the configured MCP tools by, checking the `[[mcp]]` entries.
    fn mcp_tool_names(&self) -> Result<Vec<String>> {
        let mut servers = HashSet::new();
        let mut names = Vec::new();
        for server in &self.mcp {
            let name = server.name.as_str();
            check_server_name(name).map_err(anyhow::Error::msg)?;
            ensure!(servers.insert(name), "more than one MCP server is named {name:?}");
            ensure!(!server.url.trim().is_empty(), "MCP server {name}: url is empty");
            ensure!(!server.tools.is_empty(), "MCP server {name}: tools lists no tool");
            ensure!(
                server.token_env.as_deref().is_none_or(|env| !env.trim().is_empty()),
                "MCP server {name}: tokenEnv is empty"
            );
            ensure!(
                server.timeout_secs != Some(0),
                "MCP server {name}: timeoutSecs must be greater than zero"
            );
            ensure!(
                server.max_timeout_secs != Some(0),
                "MCP server {name}: maxTimeoutSecs must be greater than zero"
            );
            for tool in &server.allow_tools {
                ensure!(
                    server.tools.contains(tool),
                    "MCP server {name}: allowTools names {tool:?}, which tools does not list"
                );
            }
            let first = names.len();
            for tool in &server.tools {
                let model_name = mcp_tool_name(name, tool).map_err(anyhow::Error::msg)?;
                ensure!(
                    !names[first..].contains(&model_name),
                    "MCP server {name}: tool {tool:?} would be named {model_name:?}, like another of its tools"
                );
                names.push(model_name);
            }
        }
        Ok(names)
    }

    /// Check names, rule patterns and skill registrations without reading skill bodies.
    pub fn validate(&self) -> Result<()> {
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
        let mcp_tools = self.mcp_tool_names()?;
        for tool in self.rules.ask_tools.iter().chain(&self.rules.deny_tools) {
            ensure!(
                known_tool(tool) || mcp_tools.contains(tool),
                "unknown tool in rule {tool:?}"
            );
        }
        self.compaction.validate()?;
        self.tool_output.validate().context("[toolOutput]")?;
        for (key, value) in [
            ("container.image", &self.container.image),
            ("container.user", &self.container.user),
            ("container.memory", &self.container.memory),
            ("container.network", &self.container.network),
        ] {
            ensure!(
                value.as_deref().is_none_or(|value| !value.trim().is_empty()),
                "{key} must not be empty"
            );
        }
        ensure!(
            self.container.pids_limit != Some(0),
            "container.pidsLimit must be greater than zero"
        );
        if let Some(network) = &self.container.network {
            check_network(network).context("container.network")?;
        }
        ensure!(
            self.execution == Execution::Local || self.container.image.is_some(),
            "execution = \"container\" needs container.image"
        );
        if self.rules.approve_all {
            ensure!(
                self.execution == Execution::Container,
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
