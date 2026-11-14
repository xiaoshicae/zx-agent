//! Extensions: [`Extension`] bundles tools, policies, hooks, system prompt sections and slash
//! commands, which an agent adds to each session it opens.
//!
//! - `command`: the slash commands extensions register

mod command;

use std::collections::HashSet;
use std::sync::Arc;

use crate::ai::PREAMBLE;
use crate::hooks::{Hook, HookStage};
use crate::policy::ToolPolicy;
use crate::prompt::GENERATED_SECTIONS;
use crate::runner::LoopConfig;
use crate::tools::{Tool, Toolbox};

pub use command::{CommandContext, CommandError, CommandOutcome, SlashCommand, check_command_name};

/// Tools, hooks and system prompt sections registered together, such as a client for an
/// external tool server. An agent takes its extensions in
/// [`AgentConfig::extensions`](crate::AgentConfig::extensions) and registers them with each
/// session it opens: the tools are shared by the sessions, and `hooks` is called once for
/// each session, and once more when the agent is made, which checks the extension.
///
/// ```
/// use std::sync::Arc;
///
/// use zx_ai::{Provider, ToolCall};
/// use zx_core::extensions::Extension;
/// use zx_core::hooks::{Hook, HookContext, HookResult};
/// use zx_core::tools::ToolOutput;
/// use zx_core::{Agent, AgentConfig};
///
/// /// Marks every tool result and tells the model so.
/// struct Checked;
///
/// struct Mark;
///
/// #[async_trait::async_trait]
/// impl Hook for Mark {
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
///     fn hooks(&self) -> Vec<Box<dyn Hook>> {
///         vec![Box::new(Mark)]
///     }
/// }
///
/// fn agent(provider: Arc<dyn Provider>) -> anyhow::Result<Agent> {
///     Agent::new(AgentConfig {
///         extensions: vec![Arc::new(Checked)],
///         ..AgentConfig::new(provider, "model-id")
///     })
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

    /// Policies that decide whether a tool call may run, registered under the extension's
    /// name. The strictest verdict of all policies stands, so an extension can tighten what
    /// the host allows but never loosen it.
    fn policies(&self) -> Vec<Arc<dyn ToolPolicy>> {
        Vec::new()
    }

    /// Hooks, each declaring [`HookStage::Extension`]: redaction runs after them, so an
    /// extension cannot bring back a redacted secret.
    fn hooks(&self) -> Vec<Box<dyn Hook>> {
        Vec::new()
    }

    /// Slash commands for a frontend (pi: `registerCommand`). Names follow
    /// [`check_command_name`] and must differ from every other registered command's.
    fn commands(&self) -> Vec<Arc<dyn SlashCommand>> {
        Vec::new()
    }
}

/// Add `extensions` to `config`, in order: their tools after the existing ones, their policies
/// and hooks under each extension's name, their sections at the end of the system prompt, their
/// commands after the existing ones. Everything is checked first, so on error `config` is
/// unchanged.
///
/// Errors: a blank or repeated extension name, a hook outside [`HookStage::Extension`], a
/// tool name already in use or a tool that the session's tool set refuses, a section name
/// that is malformed, reserved, already in the prompt or repeated, and a command name that is
/// malformed or already registered.
pub(crate) fn register(config: &mut LoopConfig, extensions: &[Arc<dyn Extension>]) -> Result<(), String> {
    if extensions.is_empty() {
        return Ok(());
    }
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
    // The tool set's own check covers names, declarations and panics.
    let mut tools: Vec<Arc<dyn Tool>> = config.tools.tools().cloned().collect();
    for part in &mut parts {
        tools.append(&mut part.tools);
    }
    config.tools = Toolbox::new(tools).map_err(|error| format!("extension tools: {error}"))?;
    for part in parts {
        for policy in part.policies {
            config.policies.push(part.name, policy);
        }
        for hook in part.hooks {
            config.hooks.push_extension_hook(part.name, hook);
        }
        for (section, content) in part.sections {
            config.system_prompt = std::mem::take(&mut config.system_prompt).with_section(&section, &content);
        }
        config.commands.extend(part.commands);
    }
    Ok(())
}

/// What one extension adds, collected before anything is registered.
struct Parts<'a> {
    name: &'a str,
    tools: Vec<Arc<dyn Tool>>,
    policies: Vec<Arc<dyn ToolPolicy>>,
    hooks: Vec<Box<dyn Hook>>,
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
/// others when the session's tool set is made.
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
        if config.system_prompt.has_section(section) || !claimed.sections.insert(section.clone()) {
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
        policies: extension.policies(),
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
