//! Extensions: [`Extension`] bundles tools, policies, hooks, system prompt sections and slash
//! commands, which an agent takes from it once and adds to every session.
//!
//! - `command`: the slash commands extensions register

mod command;

use std::collections::HashSet;
use std::sync::Arc;

use crate::ai::{PREAMBLE, SystemPrompt};
use crate::hooks::{Hook, HookChain, HookStage};
use crate::policy::{PolicyChain, ToolPolicy};
use crate::tools::Tool;

pub use command::{CommandContext, CommandOutcome, SlashCommand, check_command_name};

/// Tools, hooks and system prompt sections registered together, such as a client for an
/// external tool server. An agent takes its extensions in
/// [`AgentConfig::extensions`](crate::AgentConfig::extensions) and asks each once, when the
/// agent is made, for what it adds; every session of the agent then has it, and the sessions
/// share the tools, the policies, the hooks and the commands. A hook that keeps something for
/// one session keeps it by the session ID its context names.
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
///     fn hooks(&self) -> Vec<Arc<dyn Hook>> {
///         vec![Arc::new(Mark)]
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
    /// one the host's session setup gives
    /// ([`SessionSetup::section_names`](crate::agent::SessionSetup::section_names)), or one the
    /// prompt already has. Blank content is left out.
    fn prompt_sections(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Policies that decide whether a tool call may run, registered under the extension's
    /// name. The strictest verdict of all policies stands, so an extension can tighten what
    /// the host allows but never loosen it.
    fn policies(&self) -> Vec<Arc<dyn ToolPolicy>> {
        Vec::new()
    }

    /// Hooks, each in the stage of every hook that declares no other,
    /// [`HookStage::Default`]: redaction runs after them, so an extension cannot bring back a
    /// redacted secret.
    fn hooks(&self) -> Vec<Arc<dyn Hook>> {
        Vec::new()
    }

    /// Slash commands for a frontend (pi: `registerCommand`). Names follow
    /// [`check_command_name`] and must differ from every other registered command's.
    fn commands(&self) -> Vec<Arc<dyn SlashCommand>> {
        Vec::new()
    }
}

/// What an agent's extensions add to each of its sessions, taken from them once, when the
/// agent is made.
pub(crate) struct Expanded {
    pub(crate) tools: Vec<Arc<dyn Tool>>,
    /// Each under the name of the extension that gave it.
    pub(crate) policies: PolicyChain,
    /// Each under the name of the extension that gave it.
    pub(crate) hooks: HookChain,
    pub(crate) commands: Vec<Arc<dyn SlashCommand>>,
    sections: Vec<Section>,
}

/// A system prompt section, and the extension that gave it.
struct Section {
    extension: String,
    name: String,
    content: String,
}

impl Expanded {
    /// Take what `extensions` add, in their order, and check what does not depend on a
    /// session: the tools are checked with the other tools of a session, and the sections
    /// against the prompt they are added to ([`Self::sections_after`]).
    ///
    /// Errors: a blank or repeated extension name, a hook outside [`HookStage::Default`],
    /// a section name that is malformed, repeated, the preamble's or one of `reserved`, and a
    /// command name that is malformed or repeated.
    pub(crate) fn of(extensions: &[Arc<dyn Extension>], reserved: &[String]) -> Result<Self, String> {
        let mut expanded = Self {
            tools: Vec::new(),
            policies: PolicyChain::new(),
            hooks: HookChain::new(),
            commands: Vec::new(),
            sections: Vec::new(),
        };
        let mut claimed = Claimed::default();
        for extension in extensions {
            expanded.take(extension.as_ref(), &mut claimed, reserved)?;
        }
        Ok(expanded)
    }

    /// Check one extension's name, hooks, sections and commands against what the earlier
    /// ones claimed, and add what it gives.
    fn take<'a>(
        &mut self,
        extension: &'a dyn Extension,
        claimed: &mut Claimed<'a>,
        reserved: &[String],
    ) -> Result<(), String> {
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
            .find(|stage| *stage != HookStage::Default)
        {
            return Err(format!(
                "extension {name:?}: a hook declares the {stage:?} stage; extension hooks run in the Default stage"
            ));
        }
        let sections = extension.prompt_sections();
        for (section, _) in &sections {
            check_section_name(section, reserved).map_err(|reason| format!("extension {name:?}: {reason}"))?;
            if !claimed.sections.insert(section.clone()) {
                return Err(taken_section(name, section));
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
        self.tools.extend(extension.tools());
        for policy in extension.policies() {
            self.policies.push(name, policy);
        }
        for hook in hooks {
            self.hooks.push(name, hook);
        }
        self.sections
            .extend(sections.into_iter().map(|(section, content)| Section {
                extension: name.to_string(),
                name: section,
                content,
            }));
        self.commands.extend(commands);
        Ok(())
    }

    /// `prompt` followed by the extensions' sections, in order; blank ones are left out. An
    /// error names the extension whose section `prompt` already has.
    pub(crate) fn sections_after(&self, prompt: SystemPrompt) -> Result<SystemPrompt, String> {
        self.sections.iter().try_fold(prompt, |prompt, section| {
            if prompt.has_section(&section.name) {
                return Err(taken_section(&section.extension, &section.name));
            }
            Ok(prompt.with_section(&section.name, &section.content))
        })
    }
}

fn taken_section(extension: &str, section: &str) -> String {
    format!("extension {extension:?}: the system prompt already has a section named {section:?}")
}

/// The names the extensions taken so far have claimed, so that a repeat is an error.
#[derive(Default)]
struct Claimed<'a> {
    names: HashSet<&'a str>,
    sections: HashSet<String>,
    commands: HashSet<String>,
}

/// A section name renders as a tag (`<name>`). The preamble, and the sections the host's
/// session setup gives (`reserved`), hold what the host provides, so an extension's section
/// may not take or imitate them.
fn check_section_name(name: &str, reserved: &[String]) -> Result<(), String> {
    let mut chars = name.chars();
    let well_formed = chars.next().is_some_and(|first| first.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !well_formed {
        return Err(format!(
            "section name {name:?} must be ASCII lowercase letters, digits and _, starting with a letter"
        ));
    }
    if name == PREAMBLE || reserved.iter().any(|taken| taken == name) {
        return Err(format!("section name {name:?} is reserved for the host's own sections"));
    }
    Ok(())
}
