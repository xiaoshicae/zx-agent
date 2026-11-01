//! File-based scene configuration, outside the agent loop. Loading is synchronous,
//! bounded startup I/O; tool and frontend assembly remains the caller's responsibility.

mod context;
pub mod skills;

use std::fs::OpenOptions;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

use crate::presets::{assistant, troubleshoot};
use crate::runner::Limits;
use crate::tools::BUILTIN_TOOL_NAMES;

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
    /// Registered tool names whose calls require a host approval.
    pub ask_tools: Vec<String>,
    /// Tool names denied regardless of enabled tools or approval grants.
    pub deny_tools: Vec<String>,
}

/// Which project instructions to read at startup. Explicit paths are absolute after load.
#[derive(Debug)]
pub struct ProjectContext {
    /// Discover root-to-cwd AGENTS.md files, bounded by the nearest Git root.
    pub discover: bool,
    /// Extra instruction files, resolved relative to the profile file at load time.
    pub files: Vec<PathBuf>,
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
        Ok(())
    }

    /// Check names, rule patterns and skill registrations without reading skill bodies.
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.name.trim().is_empty(), "profile name is empty");
        for name in &self.tools {
            ensure!(
                name == skills::TOOL_NAME || BUILTIN_TOOL_NAMES.contains(&name.as_str()),
                "unknown tool {name:?}"
            );
        }
        for pattern in self.rules.allow.iter().chain(&self.rules.deny) {
            ensure!(!pattern.trim().is_empty(), "empty command rule is not allowed");
        }
        for tool in self.rules.ask_tools.iter().chain(&self.rules.deny_tools) {
            ensure!(
                tool == skills::TOOL_NAME || BUILTIN_TOOL_NAMES.contains(&tool.as_str()),
                "unknown tool in rule {tool:?}"
            );
        }
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
    // Deserialization validates the only implemented backend; no backend is ignored.
    #[serde(rename = "execution")]
    _execution: Option<Execution>,
    rules: RawRules,
    limits: RawLimits,
    extensions: RawExtensions,
    context: RawContext,
    skills: Option<Vec<Skill>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Execution {
    Local,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct RawRules {
    allow: Option<Vec<String>>,
    deny: Vec<String>,
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
    let mut options = OpenOptions::new();
    options.read(true);
    // Inspect the opened descriptor without blocking if a path is a FIFO.
    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK);
    let file = options
        .open(path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let meta = file
        .metadata()
        .with_context(|| format!("cannot inspect {}", path.display()))?;
    ensure!(meta.is_file(), "{} is not a regular file", path.display());
    ensure!(
        meta.len() <= MAX_FILE_BYTES,
        "{} exceeds {MAX_FILE_BYTES} bytes",
        path.display()
    );
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("cannot read {}", path.display()))?;
    ensure!(
        u64::try_from(bytes.len())? <= MAX_FILE_BYTES,
        "{} exceeds {MAX_FILE_BYTES} bytes",
        path.display()
    );
    String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", path.display()))
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
            "execution = 'container'",
            "[extensions]\nenabled = ['mcp']",
            "extends = 'other.toml'",
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
    fn invalid_tool_names_and_empty_rules_are_configuration_errors() {
        for text in [
            "tools = ['missing']",
            "[rules]\naskTools = ['missing']",
            "[rules]\ndeny = ['  ']",
        ] {
            assert!(load_file(text).is_err(), "{text}");
        }
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
}
