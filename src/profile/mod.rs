//! File-based scene configuration, outside the agent loop. Loading is synchronous,
//! bounded startup I/O; tool and frontend assembly remains the caller's responsibility.

mod context;
pub mod skills;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

use crate::mcp::{check_server_name, tool_name as mcp_tool_name};
use crate::presets::{assistant, troubleshoot};
use crate::runner::Limits;
use crate::text::read_text_file;
use crate::tools::BUILTIN_TOOL_NAMES;
use crate::{compaction, output_store};

use skills::Skill;

/// Maximum size of a profile, instructions, context file or skill body.
pub const MAX_FILE_BYTES: u64 = 64 * 1024;

/// Embedded baseline; custom profiles may extend one of these, never arbitrary files.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Builtin {
    Troubleshoot,
    Assistant,
}

impl Builtin {
    fn source(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Troubleshoot => (
                include_str!("../../profiles/troubleshoot.toml"),
                "troubleshoot.md",
                troubleshoot::INSTRUCTIONS,
            ),
            Self::Assistant => (
                include_str!("../../profiles/assistant.toml"),
                "assistant.md",
                assistant::INSTRUCTIONS,
            ),
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
    /// Registered tool names whose calls require a host approval.
    pub ask_tools: Vec<String>,
    /// Tool names denied regardless of enabled tools or approval grants.
    pub deny_tools: Vec<String>,
}

/// An external MCP server whose listed tools the model may call (docs/p7-mcp-review.md). The
/// frontends connect it at startup; whether a call runs is up to the rules.
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
    /// Bound for one call, in seconds; unset uses `--tool-timeout-secs`.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
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
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContainerSettings {
    /// Required in container mode; the image must already be present locally.
    pub image: Option<String>,
    /// `uid:gid` that commands run as.
    pub user: Option<String>,
    /// Memory limit in Docker's syntax, such as `1g`.
    pub memory: Option<String>,
    /// Maximum number of processes in the container.
    pub pids_limit: Option<u32>,
}

/// Fully layered scene configuration. CLI overrides are applied by the frontend;
/// callers must keep deny rules additive and call `validate` before assembly.
#[derive(Debug)]
pub struct Profile {
    /// Human-readable scene name.
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
}

impl Profile {
    /// Load an embedded name or a TOML file over the selected default. Relative profile
    /// paths use the process working directory; paths inside TOML use its parent.
    /// Unknown fields, unsupported backends and invalid files are errors.
    pub fn load(selector: Option<&str>, default: Builtin) -> Result<Self> {
        let selector = match selector {
            None => return Self::builtin(default),
            Some("troubleshoot") => return Self::builtin(Builtin::Troubleshoot),
            Some("assistant") => return Self::builtin(Builtin::Assistant),
            Some(selector) => selector,
        };
        ensure!(!selector.trim().is_empty(), "profile selector is empty");
        let path = Path::new(selector)
            .canonicalize()
            .with_context(|| format!("cannot locate profile {selector:?}"))?;
        let raw: RawProfile =
            toml::from_str(&read_text(&path)?).with_context(|| format!("invalid profile {}", path.display()))?;
        let mut profile = Self::builtin(raw.extends.unwrap_or(default))?;
        // A canonical file path always has a parent, but keep this error actionable.
        let parent = path.parent().context("profile has no parent directory")?;
        profile.overlay(raw, parent)?;
        profile.validate()?;
        Ok(profile)
    }

    fn builtin(kind: Builtin) -> Result<Self> {
        let (source, filename, instructions) = kind.source();
        let mut raw: RawProfile = toml::from_str(source).context("invalid embedded profile")?;
        ensure!(
            raw.instructions.take().as_deref() == Some(Path::new(filename)),
            "embedded instructions path is inconsistent"
        );
        let mut profile = Self {
            name: String::new(),
            instructions: instructions.into(),
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
            tool_output: output_store::Config::default(),
            execution: Execution::Local,
            container: ContainerSettings::default(),
            mcp: Vec::new(),
        };
        profile.overlay(raw, Path::new("."))?;
        profile.validate()?;
        Ok(profile)
    }

    fn overlay(&mut self, raw: RawProfile, parent: &Path) -> Result<()> {
        if let Some(name) = raw.name {
            self.name = name;
        }
        if let Some(path) = raw.instructions {
            self.instructions = read_text(&parent.join(path))?;
        }
        if let Some(tools) = raw.tools {
            self.tools = tools;
        }
        if let Some(allow) = raw.rules.allow {
            self.rules.allow = allow;
        }
        self.rules.deny.extend(raw.rules.deny);
        if let Some(prefixes) = raw.rules.auto_approve {
            self.rules.auto_approve = prefixes;
        }
        if let Some(tools) = raw.rules.ask_tools {
            self.rules.ask_tools = tools;
        }
        self.rules.deny_tools.extend(raw.rules.deny_tools);
        if let Some(turns) = raw.limits.max_turns {
            self.limits.max_turns = match turns {
                TurnLimit::Count(count) => Some(count),
                TurnLimit::Unlimited(Unlimited::Unlimited) => None,
            };
        }
        if let Some(secs) = raw.limits.timeout_secs {
            self.limits.timeout = (secs > 0).then(|| Duration::from_secs(secs));
        }
        if let Some(count) = raw.extensions.continue_on_length {
            self.continue_on_length = count;
        }
        ensure!(raw.extensions.redaction != Some(false), "redaction cannot be disabled");
        if let Some(discover) = raw.context.discover {
            self.context.discover = discover;
        }
        self.context
            .files
            .extend(raw.context.files.into_iter().map(|path| parent.join(path)));
        if let Some(skills) = raw.skills {
            self.skills = skills
                .into_iter()
                .map(|mut skill| {
                    skill.path = parent.join(skill.path);
                    skill
                })
                .collect();
        }
        if let Some(bytes) = raw.compaction.trigger_bytes {
            self.compaction.trigger_bytes = (bytes > 0).then_some(bytes);
        }
        if let Some(bytes) = raw.compaction.keep_recent_bytes {
            self.compaction.keep_recent_bytes = bytes;
        }
        if let Some(tokens) = raw.compaction.max_summary_tokens {
            self.compaction.max_summary_tokens = tokens;
        }
        if let Some(secs) = raw.compaction.timeout_secs {
            self.compaction.timeout = Duration::from_secs(secs);
        }
        if let Some(path) = raw.tool_output.directory {
            ensure!(!path.as_os_str().is_empty(), "toolOutput.directory must not be empty");
            self.tool_output.directory = parent.join(path);
        }
        if let Some(bytes) = raw.tool_output.threshold_bytes {
            self.tool_output.threshold_bytes = (bytes > 0).then_some(bytes);
        }
        if let Some(bytes) = raw.tool_output.max_file_bytes {
            self.tool_output.max_file_bytes = bytes;
        }
        if let Some(execution) = raw.execution {
            self.execution = execution;
        }
        let container = raw.container;
        for (value, layer) in [
            (&mut self.container.image, container.image),
            (&mut self.container.user, container.user),
            (&mut self.container.memory, container.memory),
        ] {
            if layer.is_some() {
                *value = layer;
            }
        }
        if container.pids_limit.is_some() {
            self.container.pids_limit = container.pids_limit;
        }
        if let Some(servers) = raw.mcp {
            self.mcp = servers;
        }
        Ok(())
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
            ensure!(
                name == skills::TOOL_NAME
                    || name == output_store::TOOL_NAME
                    || BUILTIN_TOOL_NAMES.contains(&name.as_str()),
                "unknown tool {name:?}"
            );
        }
        for pattern in self
            .rules
            .allow
            .iter()
            .chain(&self.rules.deny)
            .chain(&self.rules.auto_approve)
        {
            ensure!(!pattern.trim().is_empty(), "empty command rule is not allowed");
        }
        let mcp_tools = self.mcp_tool_names()?;
        for tool in self.rules.ask_tools.iter().chain(&self.rules.deny_tools) {
            ensure!(
                tool == skills::TOOL_NAME
                    || tool == output_store::TOOL_NAME
                    || BUILTIN_TOOL_NAMES.contains(&tool.as_str())
                    || mcp_tools.contains(tool),
                "unknown tool in rule {tool:?}"
            );
        }
        self.compaction.validate()?;
        self.tool_output.validate()?;
        for (key, value) in [
            ("container.image", &self.container.image),
            ("container.user", &self.container.user),
            ("container.memory", &self.container.memory),
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
        ensure!(
            self.execution == Execution::Local || self.container.image.is_some(),
            "execution = \"container\" needs container.image (or --container-image)"
        );
        skills::validate(&self.skills)
    }

    /// Read root-to-cwd project guidance and explicit files, within a total byte budget.
    pub fn context_section(&self, cwd: &Path) -> Result<String> {
        context::load(cwd, &self.context)
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct RawProfile {
    name: Option<String>,
    extends: Option<Builtin>,
    instructions: Option<PathBuf>,
    tools: Option<Vec<String>>,
    // Unknown backends fail to deserialize instead of being ignored.
    execution: Option<Execution>,
    container: RawContainer,
    rules: RawRules,
    limits: RawLimits,
    extensions: RawExtensions,
    context: RawContext,
    skills: Option<Vec<Skill>>,
    compaction: RawCompaction,
    tool_output: RawToolOutput,
    mcp: Option<Vec<McpServer>>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct RawCompaction {
    trigger_bytes: Option<usize>,
    keep_recent_bytes: Option<usize>,
    max_summary_tokens: Option<u32>,
    timeout_secs: Option<u64>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct RawToolOutput {
    directory: Option<PathBuf>,
    threshold_bytes: Option<usize>,
    max_file_bytes: Option<usize>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct RawContainer {
    image: Option<String>,
    user: Option<String>,
    memory: Option<String>,
    pids_limit: Option<u32>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct RawRules {
    allow: Option<Vec<String>>,
    deny: Vec<String>,
    auto_approve: Option<Vec<String>>,
    ask_tools: Option<Vec<String>>,
    deny_tools: Vec<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct RawLimits {
    max_turns: Option<TurnLimit>,
    timeout_secs: Option<u64>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum TurnLimit {
    Count(u32),
    Unlimited(Unlimited),
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Unlimited {
    Unlimited,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct RawExtensions {
    redaction: Option<bool>,
    continue_on_length: Option<u32>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct RawContext {
    discover: Option<bool>,
    files: Vec<PathBuf>,
}

/// Read a regular UTF-8 configuration file within [`MAX_FILE_BYTES`]. This is bounded,
/// synchronous startup I/O; invalid files and read failures preserve their path in errors.
pub fn read_text(path: &Path) -> Result<String> {
    read_text_file(path, MAX_FILE_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_file(text: &str) -> Result<Profile> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profile.toml");
        std::fs::write(&path, text).unwrap();
        Profile::load(Some(path.to_str().unwrap()), Builtin::Troubleshoot)
    }

    #[test]
    fn embedded_profiles_preserve_the_existing_scene_defaults() {
        let profile = Profile::load(None, Builtin::Troubleshoot).unwrap();
        assert_eq!(profile.instructions, troubleshoot::INSTRUCTIONS);
        assert_eq!(profile.limits.max_turns, Some(troubleshoot::MAX_TURNS));
        assert_eq!(
            profile.limits.timeout,
            Some(Duration::from_secs(troubleshoot::TIMEOUT_SECS))
        );
        assert_eq!(profile.continue_on_length, troubleshoot::CONTINUE_ON_LENGTH);
        let assistant = Profile::load(Some("assistant"), Builtin::Troubleshoot).unwrap();
        assert_eq!(assistant.instructions, assistant::INSTRUCTIONS);
        assert_eq!(assistant.limits.max_turns, None);
        assert_eq!(assistant.limits.timeout, None);
        assert_eq!(assistant.continue_on_length, 0);
        assert_eq!(assistant.rules.ask_tools, ["bash"]);
        assert!(
            assistant
                .rules
                .auto_approve
                .iter()
                .any(|prefix| prefix == "kubectl get")
        );
        assert!(profile.rules.auto_approve.is_empty());
        assert_eq!(profile.compaction.trigger_bytes, None);
        assert_eq!(assistant.compaction.trigger_bytes, Some(256 * 1024));
        assert_eq!(assistant.compaction.keep_recent_bytes, 64 * 1024);
        assert_eq!(assistant.tool_output.threshold_bytes, Some(8 * 1024));
    }

    #[test]
    fn only_the_interactive_profile_discovers_project_instructions_by_default() {
        assert!(!Profile::load(None, Builtin::Troubleshoot).unwrap().context.discover);
        assert!(Profile::load(None, Builtin::Assistant).unwrap().context.discover);
    }

    #[test]
    fn a_file_extends_an_embedded_baseline_and_resolves_its_instructions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("prompt.md"), "自定义说明").unwrap();
        let path = dir.path().join("scene.toml");
        std::fs::write(&path, "extends = 'assistant'\nname = 'custom'\ninstructions = 'prompt.md'\ntools = []\n[rules]\ndeny = ['secret']").unwrap();
        let profile = Profile::load(Some(path.to_str().unwrap()), Builtin::Troubleshoot).unwrap();
        assert_eq!(profile.name, "custom");
        assert_eq!(profile.instructions, "自定义说明");
        assert!(profile.tools.is_empty());
        assert_eq!(profile.rules.deny, ["secret"]);
        assert_eq!(profile.rules.ask_tools, ["bash"]);
        assert_eq!(profile.limits.max_turns, None);
    }

    #[test]
    fn zero_turns_and_unlimited_are_distinct_configurations() {
        assert_eq!(load_file("[limits]\nmaxTurns = 0").unwrap().limits.max_turns, Some(0));
        assert_eq!(
            load_file("[limits]\nmaxTurns = 'unlimited'").unwrap().limits.max_turns,
            None
        );
        assert_eq!(load_file("[limits]\ntimeoutSecs = 0").unwrap().limits.timeout, None);
    }

    #[test]
    fn unknown_fields_and_invalid_budget_values_are_rejected() {
        for text in [
            "typo = true",
            "[limits]\nmax_turns = 2",
            "[limits]\nmaxTurns = -1",
            "[limits]\nmaxTurns = 'infinite'",
        ] {
            assert!(load_file(text).is_err(), "{text}");
        }
    }

    #[test]
    fn unsupported_execution_and_extensions_fail_instead_of_being_ignored() {
        for text in [
            "execution = 'kubernetes'",
            "[extensions]\nenabled = ['mcp']",
            "extends = 'other.toml'",
        ] {
            assert!(load_file(text).is_err(), "{text}");
        }
    }

    #[test]
    fn container_execution_reads_its_settings_and_defaults_to_local() {
        assert_eq!(load_file("").unwrap().execution, Execution::Local);
        let profile = load_file(
            "execution = 'container'\n[container]\nimage = 'debian:bookworm-slim'\nuser = '1000:1000'\nmemory = '512m'\npidsLimit = 64",
        )
        .unwrap();
        assert_eq!(profile.execution, Execution::Container);
        assert_eq!(
            profile.container,
            ContainerSettings {
                image: Some("debian:bookworm-slim".into()),
                user: Some("1000:1000".into()),
                memory: Some("512m".into()),
                pids_limit: Some(64),
            }
        );
    }

    #[test]
    fn invalid_container_settings_are_configuration_errors() {
        for text in [
            "execution = 'container'",
            "execution = 'container'\n[container]\nimage = ' '",
            "[container]\nimage = 'x'\npidsLimit = 0",
            "[container]\nimage = 'x'\nuser = ''",
            "[container]\nimage = 'x'\nnetwork = 'host'",
        ] {
            assert!(load_file(text).is_err(), "{text}");
        }
    }

    #[test]
    fn redaction_cannot_be_removed_by_a_profile() {
        assert!(
            load_file("[extensions]\nredaction = false")
                .unwrap_err()
                .to_string()
                .contains("cannot be disabled")
        );
    }

    #[test]
    fn context_management_settings_overlay_individually_and_validate() {
        let profile = load_file("extends = 'assistant'\n[compaction]\ntriggerBytes = 0\nmaxSummaryTokens = 1024\n[toolOutput]\nthresholdBytes = 0").unwrap();
        assert_eq!(profile.compaction.trigger_bytes, None);
        assert_eq!(profile.compaction.keep_recent_bytes, 64 * 1024);
        assert_eq!(profile.compaction.max_summary_tokens, 1024);
        assert_eq!(profile.tool_output.threshold_bytes, None);
        for text in [
            "[compaction]\nkeepRecentBytes = 0",
            "[compaction]\ntimeoutSecs = 0",
            "[compaction]\nunknown = 1",
            "[toolOutput]\ndirectory = ''",
            "[toolOutput]\nthreshold_bytes = 256",
        ] {
            assert!(load_file(text).is_err(), "{text}");
        }
    }

    #[test]
    fn invalid_tool_names_and_empty_rules_are_configuration_errors() {
        for text in [
            "tools = ['missing']",
            "[rules]\naskTools = ['missing']",
            "[rules]\ndeny = ['  ']",
            "[rules]\nautoApprove = ['']",
        ] {
            assert!(load_file(text).is_err(), "{text}");
        }
    }

    #[test]
    fn a_file_replaces_the_auto_approve_list_of_its_baseline() {
        let profile = load_file("extends = 'assistant'\n[rules]\nautoApprove = ['kubectl get']").unwrap();
        assert_eq!(profile.rules.auto_approve, ["kubectl get"]);
        let profile = load_file("extends = 'assistant'\n[rules]\nautoApprove = []").unwrap();
        assert!(profile.rules.auto_approve.is_empty());
    }

    #[test]
    fn bounded_reads_accept_the_exact_limit_and_reject_oversize_and_invalid_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("text");
        let exact = "x".repeat(64 * 1024);
        std::fs::write(&path, &exact).unwrap();
        assert_eq!(read_text(&path).unwrap(), exact);
        std::fs::write(&path, format!("{exact}界")).unwrap();
        assert!(read_text(&path).unwrap_err().to_string().contains("exceeds"));
        std::fs::write(&path, [0xff]).unwrap();
        assert!(read_text(&path).unwrap_err().to_string().contains("UTF-8"));
        assert!(
            read_text(dir.path())
                .unwrap_err()
                .to_string()
                .contains("not a regular file")
        );
    }

    #[cfg(unix)]
    #[test]
    fn special_files_are_refused_before_reading() {
        assert!(
            read_text(Path::new("/dev/null"))
                .unwrap_err()
                .to_string()
                .contains("not a regular file")
        );
    }

    #[test]
    fn the_shipped_review_example_resolves_its_context_and_skill_files() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/profiles/review.toml");
        let profile = Profile::load(Some(path.to_str().unwrap()), Builtin::Troubleshoot).unwrap();
        assert_eq!(profile.tools, ["read_file", "read_skill"]);
        assert_eq!(profile.rules.deny_tools, ["bash"]);
        assert!(
            profile
                .context_section(path.parent().unwrap())
                .unwrap()
                .contains("This example profile")
        );
        assert_eq!(profile.skills[0].name, "rust-review");
        assert!(read_text(&profile.skills[0].path).unwrap().contains("Rust review"));
    }

    const PROM: &str = "[[mcp]]\nname = 'prom'\nurl = 'https://prom.example/mcp'\ntokenEnv = 'PROM_TOKEN'\n\
                        tools = ['query', 'range']\nallowTools = ['query']\ntimeoutSecs = 30\n";

    #[test]
    fn mcp_servers_are_read_from_the_profile() {
        let profile = load_file(PROM).unwrap();
        assert_eq!(
            profile.mcp,
            [McpServer {
                name: "prom".into(),
                url: "https://prom.example/mcp".into(),
                token_env: Some("PROM_TOKEN".into()),
                tools: vec!["query".into(), "range".into()],
                allow_tools: vec!["query".into()],
                timeout_secs: Some(30),
            }]
        );
        assert!(Profile::load(None, Builtin::Assistant).unwrap().mcp.is_empty());
    }

    #[test]
    fn rules_may_name_the_configured_mcp_tools_only() {
        let profile = load_file(&format!("[rules]\ndenyTools = ['prom__range']\n{PROM}")).unwrap();
        assert_eq!(profile.rules.deny_tools, ["prom__range"]);
        let error = load_file(&format!("[rules]\naskTools = ['prom__admin']\n{PROM}"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown tool in rule \"prom__admin\""), "{error}");
    }

    #[test]
    fn malformed_mcp_settings_are_refused() {
        let server = |fields: &str| format!("[[mcp]]\nname = 'prom'\nurl = 'https://prom.example/mcp'\n{fields}\n");
        for (text, expected) in [
            (server("tools = []"), "tools lists no tool"),
            (
                server("tools = ['query']\nallowTools = ['range']"),
                "which tools does not list",
            ),
            (server("tools = ['query']\ntimeoutSecs = 0"), "greater than zero"),
            (server("tools = ['query']\ntokenEnv = ' '"), "tokenEnv is empty"),
            (server("tools = ['a.b', 'a_b']"), "like another of its tools"),
            (server(&format!("tools = ['{}']", "x".repeat(64))), "longer than 64"),
            (
                format!("{}{}", server("tools = ['query']"), server("tools = ['range']")),
                "more than one MCP server",
            ),
            (
                "[[mcp]]\nname = 'Prom'\nurl = 'https://prom.example/mcp'\ntools = ['query']\n".to_string(),
                "must be ASCII lowercase",
            ),
            (server("tools = ['query']\nheaders = {}"), "unknown field"),
        ] {
            let error = format!("{:#}", load_file(&text).unwrap_err());
            assert!(error.contains(expected), "{text}\n{error}");
        }
    }
}
