//! Extensions: [`Extension`] bundles tools, hooks and system prompt sections, and
//! [`register`] adds them to a loop.

use std::collections::HashSet;
use std::sync::Arc;

use crate::agent::{HookStage, Hooks, LoopConfig, SlashCommand, Tool, check_command_name};
use crate::ai::PREAMBLE;
use crate::prompt::GENERATED_SECTIONS;

/// Tools, hooks and system prompt sections registered together, such as a client for an
/// external tool server. A host may register one extension into many loops, as serve does
/// for each chat thread: the tools are shared, and `hooks` is called once per registration.
///
/// ```
/// use std::sync::Arc;
///
/// use zx_ai::ToolCall;
/// use zx_core::agent::{HookContext, HookResult, Hooks, LoopConfig, ToolOutput};
/// use zx_core::extensions::{Extension, register};
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

    /// Slash commands for a frontend (pi: `registerCommand`). Names follow
    /// [`check_command_name`] and must differ from every other registered command's.
    fn commands(&self) -> Vec<Arc<dyn SlashCommand>> {
        Vec::new()
    }
}

/// Add `extensions` to `config`, in order: their tools after the existing ones, their hooks
/// under each extension's name, their sections at the end of the system prompt, their
/// commands after the existing ones. Everything is checked first, so on error `config` is
/// unchanged.
///
/// Errors: a blank or repeated extension name, a hook outside [`HookStage::Extension`], a
/// tool name already in use or a tool that fails [`LoopSpec::validate`](crate::agent::LoopSpec::validate),
/// a section name that is malformed, reserved, already in the prompt or repeated, and a
/// command name that is malformed or already registered.
pub fn register(config: &mut LoopConfig, extensions: &[Arc<dyn Extension>]) -> Result<(), String> {
    let mut claimed = Claimed {
        names: HashSet::new(),
        sections: HashSet::new(),
        commands: config
            .commands
            .iter()
            .map(|command| command.name().to_string())
            .collect(),
    };
    let mut parts = Vec::with_capacity(extensions.len());
    for extension in extensions {
        parts.push(check_extension(config, extension.as_ref(), &mut claimed)?);
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
        config.commands.extend(part.commands);
    }
    Ok(())
}

/// What one extension adds, collected before anything is registered.
struct Parts<'a> {
    name: &'a str,
    tools: Vec<Arc<dyn Tool>>,
    hooks: Vec<Box<dyn Hooks>>,
    sections: Vec<(String, String)>,
    commands: Vec<Arc<dyn SlashCommand>>,
}

/// The names the extensions checked so far have claimed, so that a repeat is an error;
/// `commands` starts with the commands `config` already has.
struct Claimed<'a> {
    names: HashSet<&'a str>,
    sections: HashSet<String>,
    commands: HashSet<String>,
}

/// Check one extension's name, hooks, sections and commands against `config` and what
/// earlier extensions claimed, and collect what it adds. Its tools are checked with the
/// others by [`LoopSpec::validate`](crate::agent::LoopSpec::validate).
fn check_extension<'a>(
    config: &LoopConfig,
    extension: &'a dyn Extension,
    claimed: &mut Claimed<'a>,
) -> Result<Parts<'a>, String> {
    let name = extension.name();
    if name.trim().is_empty() {
        return Err("an extension has a blank name".to_string());
    }
    if !claimed.names.insert(name) {
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
        if config.spec.system_prompt.has_section(section) || !claimed.sections.insert(section.clone()) {
            return Err(format!(
                "extension {name:?}: the system prompt already has a section named {section:?}"
            ));
        }
    }
    let commands = extension.commands();
    for command in &commands {
        let command = command.name();
        check_command_name(command).map_err(|reason| format!("extension {name:?}: {reason}"))?;
        if !claimed.commands.insert(command.to_string()) {
            return Err(format!(
                "extension {name:?}: a command named {command:?} is already registered"
            ));
        }
    }
    Ok(Parts {
        name,
        tools: extension.tools(),
        hooks,
        sections,
        commands,
    })
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
