//! Extensions: [`Extension`] bundles tools, hooks and system prompt sections, and
//! [`register`] adds them to a loop. The built-in policies below use the same Hooks
//! interface as extension hooks.

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::agent::hooks::{HookContext, HookResult, HookStage, Hooks, TurnDecision, TurnDisposition, TurnSummary};
use crate::agent::{LoopConfig, Tool, ToolOutput};
use crate::ai::{PREAMBLE, StopReason, ToolCall};
use crate::prompt::GENERATED_SECTIONS;
use crate::sync::lock;
use crate::text::read_regular_file;

/// Tools, hooks and system prompt sections registered together, such as a client for an
/// external tool server. A host may register one extension into many loops, as serve does
/// for each chat thread: the tools are shared, and `hooks` is called once per registration.
///
/// ```
/// use std::sync::Arc;
///
/// use zx_agent::agent::{HookContext, HookResult, Hooks, LoopConfig, ToolOutput};
/// use zx_agent::ai::ToolCall;
/// use zx_agent::extensions::{Extension, register};
///
/// /// Marks every tool result and tells the model so.
/// struct Checked;
///
/// struct Mark;
///
/// #[async_trait::async_trait]
/// impl Hooks for Mark {
///     async fn after_tool(&self, _call: &ToolCall, mut output: ToolOutput, _ctx: &HookContext) -> HookResult<ToolOutput> {
///         output.content.push_str("\n(checked)");
///         Ok(output)
///     }
/// }
///
/// impl Extension for Checked {
///     fn name(&self) -> &str {
///         "checked"
///     }
///
///     fn prompt_sections(&self) -> Vec<(String, String)> {
///         vec![("checked".into(), "Tool results end with (checked).".into())]
///     }
///
///     fn hooks(&self) -> Vec<Box<dyn Hooks>> {
///         vec![Box::new(Mark)]
///     }
/// }
///
/// fn add(config: &mut LoopConfig) -> Result<(), String> {
///     register(config, &[Arc::new(Checked)])
/// }
/// ```
pub trait Extension: Send + Sync {
    /// Names the extension in errors, and its hooks in `hook_failed` events and traces. Not
    /// blank, and unique among the extensions registered together.
    fn name(&self) -> &str;

    /// Tools for the model. Their names must differ from every other tool's.
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }

    /// `(name, content)` sections placed after the prompt's existing ones. A name is ASCII
    /// lowercase letters, digits and `_`, starting with a letter; it may not be the preamble's,
    /// one of [`GENERATED_SECTIONS`] or one the prompt already has. Blank content is left out.
    fn prompt_sections(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Hooks, each declaring [`HookStage::Extension`]: built-in policies decide before them
    /// and redaction runs after them, so an extension can neither loosen a policy's verdict
    /// nor bring back a redacted secret.
    fn hooks(&self) -> Vec<Box<dyn Hooks>> {
        Vec::new()
    }
}

/// Add `extensions` to `config`, in order: their tools after the existing ones, their hooks
/// under each extension's name, their sections at the end of the system prompt. Everything is
/// checked first, so on error `config` is unchanged.
///
/// Errors: a blank or repeated extension name, a hook outside [`HookStage::Extension`], a
/// tool name already in use or a tool that fails [`LoopSpec::validate`](crate::agent::LoopSpec::validate),
/// and a section name that is malformed, reserved, already in the prompt or repeated.
pub fn register(config: &mut LoopConfig, extensions: &[Arc<dyn Extension>]) -> Result<(), String> {
    let mut names = HashSet::new();
    let mut sections_seen = HashSet::new();
    let mut parts = Vec::new();
    for extension in extensions {
        let name = extension.name();
        if name.trim().is_empty() {
            return Err("an extension has a blank name".to_string());
        }
        if !names.insert(name) {
            return Err(format!("more than one extension is named {name:?}"));
        }
        let hooks = extension.hooks();
        if let Some(stage) = hooks
            .iter()
            .map(|hook| hook.stage())
            .find(|stage| *stage != HookStage::Extension)
        {
            return Err(format!(
                "extension {name:?}: a hook declares the {stage:?} stage; extension hooks run in the Extension stage"
            ));
        }
        let sections = extension.prompt_sections();
        for (section, _) in &sections {
            check_section_name(section).map_err(|reason| format!("extension {name:?}: {reason}"))?;
            if config.spec.system_prompt.has_section(section) || !sections_seen.insert(section.clone()) {
                return Err(format!(
                    "extension {name:?}: the system prompt already has a section named {section:?}"
                ));
            }
        }
        parts.push(Parts {
            name,
            tools: extension.tools(),
            hooks,
            sections,
        });
    }
    // The loop's own check covers names, declarations and panics; on failure the tools are
    // taken out again, so nothing is registered.
    let existing = config.spec.tools.len();
    for part in &mut parts {
        config.spec.tools.append(&mut part.tools);
    }
    if let Err(error) = config.spec.validate() {
        config.spec.tools.truncate(existing);
        return Err(format!("extension tools: {error}"));
    }
    for part in parts {
        for hook in part.hooks {
            config.hooks.push_extension_hook(part.name, hook);
        }
        for (section, content) in part.sections {
            config.spec.system_prompt = std::mem::take(&mut config.spec.system_prompt).with_section(&section, &content);
        }
    }
    Ok(())
}

/// What one extension adds, collected before anything is registered.
struct Parts<'a> {
    name: &'a str,
    tools: Vec<Arc<dyn Tool>>,
    hooks: Vec<Box<dyn Hooks>>,
    sections: Vec<(String, String)>,
}

/// A section name renders as a tag (`<name>`), and the ones zx-agent generates hold facts the
/// program provides, so an extension's section may not take or imitate them.
fn check_section_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let well_formed = chars.next().is_some_and(|first| first.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !well_formed {
        return Err(format!(
            "section name {name:?} must be ASCII lowercase letters, digits and _, starting with a letter"
        ));
    }
    if name == PREAMBLE || GENERATED_SECTIONS.contains(&name) {
        return Err(format!("section name {name:?} is reserved for zx-agent's own sections"));
    }
    Ok(())
}

/// The largest secret file read, such as a token: anything bigger is not a single secret, and
/// it could never be matched whole in the output of a built-in tool, which returns at most
/// 50 KiB at a time.
const MAX_SECRET_FILE_BYTES: u64 = 64 * 1024;

/// The unattended continuation text; the runtime does not depend on this wording.
pub const CONTINUE_PROMPT: &str = "Your previous response was cut off at the output token limit. Continue exactly where it \
stopped, without repeating earlier text, and keep the rest concise.";

/// Stateless continuation policy. The runtime supplies a per-run continuation count.
pub struct ContinueOnLength {
    max_continuations: u32,
}

impl ContinueOnLength {
    pub fn new(max_continuations: u32) -> Self {
        Self { max_continuations }
    }
}

#[async_trait]
impl Hooks for ContinueOnLength {
    async fn finish_turn(&self, turn: &TurnSummary<'_>, _ctx: &HookContext) -> HookResult<TurnDecision> {
        if turn.disposition == TurnDisposition::Stop
            && turn.response.stop_reason == StopReason::Length
            && turn.remaining_turns != Some(0)
            && turn.continuations < self.max_continuations
        {
            Ok(TurnDecision::ContinueAnswer(CONTINUE_PROMPT.to_string()))
        } else {
            Ok(TurnDecision::Default)
        }
    }
}

/// Earlier values read from secret files that stay redacted after the files change, oldest
/// dropped first; current values are always redacted. A rotated credential, such as a
/// ServiceAccount token, stays valid until it expires, so the values it replaced must stay
/// hidden as well; the bound keeps a long-running process from growing.
const MAX_FILE_SECRETS: usize = 32;

/// Redacts exact occurrences of configured secrets from tool output before persistence.
///
/// Empty secrets are ignored. Longer matches run first so overlapping secrets do not
/// leave a suffix visible. This does not discover unknown credentials or filter inputs.
pub struct RedactSecrets {
    secrets: Vec<String>,
    files: Vec<PathBuf>,
    /// Values read from `files`, newest last: the current ones and at most
    /// [`MAX_FILE_SECRETS`] earlier ones.
    file_secrets: Mutex<Vec<String>>,
}

impl RedactSecrets {
    pub fn new(secrets: impl IntoIterator<Item = String>) -> Self {
        Self {
            secrets: secrets.into_iter().filter(|secret| !secret.is_empty()).collect(),
            files: Vec::new(),
            file_secrets: Mutex::new(Vec::new()),
        }
    }

    /// Also redact the contents of `files`, each holding one secret such as the token of a
    /// Kubernetes ServiceAccount. A directory stands for every file directly inside it whose
    /// name does not start with a dot, so a whole Kubernetes Secret volume can be given and keys
    /// added to it later are covered. Every redaction reads them again, so a rotated value is
    /// covered and earlier values stay redacted; surrounding whitespace is ignored. Content
    /// that is not UTF-8 is matched as the built-in tools print it, decoded lossily. A file
    /// that is missing, or that this process may not read, is skipped: commands running as
    /// the same user cannot read it either. Any other failure, such as a file over 64 KiB or
    /// one that is not a regular file, fails the hook, which discards the tool output; check
    /// the files with [`check_secret_files`] before use to report those at startup.
    pub fn with_secret_files(mut self, files: impl IntoIterator<Item = PathBuf>) -> Self {
        self.files.extend(files);
        self
    }

    /// The current values of the secret files, and earlier ones within [`MAX_FILE_SECRETS`].
    fn file_secrets(&self) -> Result<Vec<String>, String> {
        let mut current = Vec::new();
        for path in &self.files {
            for file in secret_files_at(path)? {
                // Small, bounded local reads, synchronous like the other configuration reads.
                match read_regular_file(&file, MAX_SECRET_FILE_BYTES) {
                    // The built-in tools decode output the same way, so a value that is not
                    // UTF-8 appears in their output exactly like this.
                    Ok(bytes) => current.push(String::from_utf8_lossy(&bytes).trim().to_string()),
                    Err(error) if is_unreadable(&error) => {}
                    Err(error) => return Err(format!("cannot read a secret file to redact tool output: {error:#}")),
                }
            }
        }
        current.retain(|value| !value.is_empty());
        let mut seen = lock(&self.file_secrets);
        for value in &current {
            if !seen.contains(value) {
                seen.push(value.clone());
            }
        }
        // Current values always stay; of the earlier ones, the oldest go first.
        let mut earlier = seen.iter().filter(|value| !current.contains(value)).count();
        seen.retain(|value| {
            let drop = earlier > MAX_FILE_SECRETS && !current.contains(value);
            if drop {
                earlier -= 1;
            }
            !drop
        });
        // A copy, so the lock is not held while the output is rewritten.
        Ok(seen.clone())
    }
}

/// The files to read for a registered path: the path itself, or for a directory the entries
/// directly inside it whose names do not start with a dot. That skips the `..data` links that
/// Kubernetes keeps in Secret volumes; subdirectories are skipped too. A directory that is
/// missing or that this process may not list yields nothing, like an unreadable file.
fn secret_files_at(path: &Path) -> Result<Vec<PathBuf>, String> {
    if !path.is_dir() {
        // A file, or nothing at all: reading it decides, with the same rules as for files.
        return Ok(vec![path.to_path_buf()]);
    }
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::PermissionDenied) => {
            return Ok(Vec::new());
        }
        Err(error) => return Err(format!("cannot list secret files in {}: {error}", path.display())),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot list secret files in {}: {error}", path.display()))?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let file = entry.path();
        // is_dir follows links, as the entries of a Secret volume are links to the data.
        if !file.is_dir() {
            files.push(file);
        }
    }
    files.sort();
    Ok(files)
}

#[async_trait]
impl Hooks for RedactSecrets {
    /// After every extension has changed the output, so none can bring a secret back.
    fn stage(&self) -> HookStage {
        HookStage::Redaction
    }

    async fn after_tool(&self, _call: &ToolCall, mut output: ToolOutput, _ctx: &HookContext) -> HookResult<ToolOutput> {
        let file_secrets = self.file_secrets()?;
        let mut secrets: Vec<&str> = self.secrets.iter().chain(&file_secrets).map(String::as_str).collect();
        secrets.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        secrets.dedup();
        for secret in secrets {
            output.content = output.content.replace(secret, "[REDACTED]");
        }
        Ok(output)
    }
}

/// Check before use that the secret files registered at `paths`, given as to
/// [`RedactSecrets::with_secret_files`], can be redacted: each must be a regular file of at
/// most 64 KiB. A larger file could never be matched whole in the output of a built-in tool,
/// and it usually holds structured data, whose single values should be registered instead.
/// Content that is not UTF-8 is fine. Files that are missing or that this process may not
/// read are not errors here: the redaction skips them. Returns one message naming every file
/// that cannot be used; a file that only becomes unusable later fails the redaction instead.
pub fn check_secret_files(paths: &[PathBuf]) -> Result<(), String> {
    let mut problems = Vec::new();
    for path in paths {
        for file in secret_files_at(path)? {
            match read_regular_file(&file, MAX_SECRET_FILE_BYTES) {
                Ok(_) => {}
                Err(error) if is_unreadable(&error) => {}
                Err(error) => problems.push(format!("{error:#}")),
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "cannot redact these secret files from tool output: {}; register files that each hold a single value of at most {MAX_SECRET_FILE_BYTES} bytes",
            problems.join("; ")
        ))
    }
}

/// Values shorter than this many characters are still redacted, but every occurrence of the
/// same text is replaced too, wherever it appears. GitLab masks only values of at least 8
/// characters (<https://docs.gitlab.com/ci/variables/>).
pub const MIN_SECRET_CHARS: usize = 8;

/// The files at `paths`, given as to [`RedactSecrets::with_secret_files`], whose value is
/// shorter than [`MIN_SECRET_CHARS`] characters, for a notice at startup: such a value is still
/// redacted, and so is every other occurrence of the same text in tool output. Blank values,
/// which redact nothing, are left out, as are files that cannot be read: [`check_secret_files`]
/// reports those.
pub fn short_secret_files(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut short = Vec::new();
    for path in paths {
        // A directory that cannot be listed holds no value to report; check_secret_files
        // reports the failure.
        let Ok(files) = secret_files_at(path) else {
            continue;
        };
        for file in files {
            // Likewise for a file that cannot be read.
            let Ok(bytes) = read_regular_file(&file, MAX_SECRET_FILE_BYTES) else {
                continue;
            };
            let chars = String::from_utf8_lossy(&bytes).trim().chars().count();
            if chars > 0 && chars < MIN_SECRET_CHARS {
                short.push(file);
            }
        }
    }
    short
}

/// Whether a secret file is missing or not readable by this process.
fn is_unreadable(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| matches!(error.kind(), ErrorKind::NotFound | ErrorKind::PermissionDenied))
}
