//! An agent's `[permissions]`: which tools ask, which calls run without asking and which are
//! denied. Only the syntax is here; the decisions are made by the policies of
//! [`crate::policy`]: `CommandPolicy` for the commands denied, `RulePolicy` for the tools that
//! ask or are denied, and `AutoApprove` for the commands allowed.
//!
//! An entry is a tool name, built-in or MCP (`<server>__<tool>`), or `bash(<text>)`. A
//! `bash(...)` entry in `allow` matches a command that starts with the text; one in `deny`
//! matches a command that contains it anywhere, without regard to case.

use std::collections::BTreeMap;

use anyhow::{Result, bail, ensure};
use serde::Deserialize;

use crate::policy::{AutoApprove, ProgramOptions};

/// One entry of `allow` or `deny`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    /// A tool, by the name the model calls it.
    Tool(String),
    /// Bash commands, by a text: a prefix in `allow`, a substring in `deny`.
    Bash(String),
}

/// The checked `[permissions]` of an agent.
#[derive(Debug, Default)]
pub struct Permissions {
    /// Tools whose calls need a person's approval; refused where no one can answer.
    pub ask: Vec<String>,
    /// Commands that run without asking, by prefix, and MCP tools that do.
    pub allow: Vec<Entry>,
    /// Texts that keep an allowed command asking, compared without case, such as `secret`.
    pub allow_except: Vec<String>,
    /// For a program, such as kubectl, the options an allowed command may pass; any other
    /// option asks. Programs not listed may pass any option.
    pub allow_options: BTreeMap<String, ProgramOptions>,
    /// Tools denied whatever else says, and commands containing a text.
    pub deny: Vec<Entry>,
    /// Ask nobody: calls that would wait for an approval run instead, while denials and the
    /// command policy still apply. Only with container execution, where the container is the
    /// sandbox, and not together with `ask`.
    pub approve_all: bool,
}

/// `[permissions]` as a file writes it. Each key it sets replaces the inherited value whole,
/// except `allowOptions`, which replaces the entries of the programs it lists.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct RawPermissions {
    ask: Option<Vec<String>>,
    allow: Option<Vec<String>>,
    allow_except: Option<Vec<String>>,
    allow_options: BTreeMap<String, ProgramOptions>,
    deny: Option<Vec<String>>,
    approve_all: Option<bool>,
}

impl RawPermissions {
    /// Lay these values over `permissions`, checking the syntax of every entry.
    pub(super) fn apply(self, permissions: &mut Permissions) -> Result<()> {
        if let Some(ask) = self.ask {
            for tool in &ask {
                ensure!(
                    !tool.starts_with("bash("),
                    "permissions.ask: {tool:?}: ask names tools, such as \"bash\"; to let some commands run without asking, add bash(<command>) to permissions.allow"
                );
                ensure!(!tool.trim().is_empty(), "permissions.ask: an entry is empty");
            }
            permissions.ask = ask;
        }
        if let Some(allow) = self.allow {
            permissions.allow = entries("allow", allow)?;
        }
        if let Some(texts) = self.allow_except {
            ensure!(
                texts.iter().all(|text| !text.trim().is_empty()),
                "permissions.allowExcept: an entry is empty"
            );
            permissions.allow_except = texts;
        }
        permissions.allow_options.extend(self.allow_options);
        if let Some(deny) = self.deny {
            permissions.deny = entries("deny", deny)?;
        }
        if let Some(approve_all) = self.approve_all {
            permissions.approve_all = approve_all;
        }
        Ok(())
    }
}

/// The entries of `permissions.<key>`.
fn entries(key: &str, values: Vec<String>) -> Result<Vec<Entry>> {
    values.into_iter().map(|value| entry(key, value)).collect()
}

/// A tool name or `bash(<text>)`, the text not empty.
fn entry(key: &str, value: String) -> Result<Entry> {
    let trimmed = value.trim();
    ensure!(!trimmed.is_empty(), "permissions.{key}: an entry is empty");
    if let Some(inner) = trimmed.strip_prefix("bash(") {
        let Some(text) = inner.strip_suffix(')') else {
            bail!("permissions.{key}: {value:?} has no closing parenthesis; write bash(<command>)");
        };
        ensure!(
            !text.trim().is_empty(),
            "permissions.{key}: {value:?} names no command; write bash(<command>)"
        );
        return Ok(Entry::Bash(text.to_string()));
    }
    ensure!(
        !trimmed.contains(['(', ')']),
        "permissions.{key}: {value:?} is neither a tool name nor bash(<command>)"
    );
    Ok(Entry::Tool(value))
}

impl Permissions {
    /// The command prefixes that run without asking.
    pub fn allowed_commands(&self) -> Vec<String> {
        bash_texts(&self.allow)
    }

    /// The tools that run without asking: MCP tools, which otherwise ask.
    pub fn allowed_tools(&self) -> Vec<String> {
        tool_names(&self.allow)
    }

    /// The texts whose commands are denied.
    pub fn denied_commands(&self) -> Vec<String> {
        bash_texts(&self.deny)
    }

    /// The tools that are denied.
    pub fn denied_tools(&self) -> Vec<String> {
        tool_names(&self.deny)
    }

    /// The bash commands these permissions let run without asking, for the approval rules.
    pub fn auto_approval(&self) -> AutoApprove {
        self.allow_options.iter().fold(
            AutoApprove::new(self.allowed_commands()).except(&self.allow_except),
            |list, (program, options)| list.options(program, options.clone()),
        )
    }

    /// Check the tool names against what is registered: `known` for the host's own tools and
    /// `mcp` for the configured MCP tools, and the program options.
    pub(super) fn validate(&self, known: impl Fn(&str) -> bool, mcp: &[String]) -> Result<()> {
        let registered = |tool: &str| known(tool) || mcp.iter().any(|name| name == tool);
        for tool in &self.ask {
            ensure!(registered(tool), "permissions.ask: unknown tool {tool:?}");
        }
        for tool in self.allowed_tools() {
            ensure!(
                !known(&tool),
                "permissions.allow: {tool} runs without asking unless it is in permissions.ask"
            );
            ensure!(registered(&tool), "permissions.allow: unknown tool {tool:?}");
            ensure!(
                !self.ask.contains(&tool),
                "{tool} is in both permissions.ask and permissions.allow"
            );
        }
        for tool in self.denied_tools() {
            ensure!(registered(&tool), "permissions.deny: unknown tool {tool:?}");
        }
        for (program, options) in &self.allow_options {
            check_program_options(program, options)?;
        }
        Ok(())
    }
}

fn bash_texts(entries: &[Entry]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            Entry::Bash(text) => Some(text.clone()),
            Entry::Tool(_) => None,
        })
        .collect()
}

fn tool_names(entries: &[Entry]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            Entry::Tool(name) => Some(name.clone()),
            Entry::Bash(_) => None,
        })
        .collect()
}

/// A program name of `permissions.allowOptions` is one lowercase word, since commands are
/// compared without case; options are written without their dashes, and a letter is either
/// a flag or takes a value.
fn check_program_options(program: &str, options: &ProgramOptions) -> Result<()> {
    ensure!(
        !program.is_empty() && !program.contains(char::is_whitespace) && program.to_lowercase() == program,
        "permissions.allowOptions: program {program:?} must be one lowercase word"
    );
    for name in &options.long {
        ensure!(
            !name.is_empty() && !name.starts_with('-') && !name.contains(|c: char| c.is_whitespace() || c == '='),
            "permissions.allowOptions.{program}: long option {name:?} must be a name without dashes, such as \"namespace\""
        );
    }
    for letter in options.short_with_value.iter().chain(&options.short_flags) {
        ensure!(
            letter.is_ascii_alphanumeric(),
            "permissions.allowOptions.{program}: short option {letter:?} must be a letter or digit"
        );
    }
    if let Some(letter) = options
        .short_flags
        .iter()
        .find(|letter| options.short_with_value.contains(letter))
    {
        bail!("permissions.allowOptions.{program}: {letter:?} cannot be both a flag and an option with a value");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Permissions> {
        let raw: RawPermissions = toml::from_str(text)?;
        let mut permissions = Permissions::default();
        raw.apply(&mut permissions)?;
        Ok(permissions)
    }

    fn known(tool: &str) -> bool {
        ["bash", "read_file", "write_file"].contains(&tool)
    }

    #[test]
    fn entries_are_tools_or_bash_texts() {
        let permissions = parse(
            "ask = ['bash']\nallow = ['bash(kubectl get)', 'prom__query']\nallowExcept = ['secret']\n\
             deny = ['bash(rm -rf)', 'write_file']",
        )
        .unwrap();
        assert_eq!(permissions.allowed_commands(), ["kubectl get"]);
        assert_eq!(permissions.allowed_tools(), ["prom__query"]);
        assert_eq!(permissions.denied_commands(), ["rm -rf"]);
        assert_eq!(permissions.denied_tools(), ["write_file"]);
        assert!(permissions.auto_approval().covers("kubectl get pods"));
        assert!(!permissions.auto_approval().covers("kubectl get secret x"));
        permissions.validate(known, &["prom__query".into()]).unwrap();
    }

    #[test]
    fn malformed_entries_name_their_field() {
        for (text, expected) in [
            ("ask = ['bash(ls)']", "permissions.ask: \"bash(ls)\": ask names tools"),
            ("ask = [' ']", "permissions.ask: an entry is empty"),
            ("allow = ['bash()']", "permissions.allow: \"bash()\" names no command"),
            (
                "allow = ['bash(  )']",
                "permissions.allow: \"bash(  )\" names no command",
            ),
            (
                "deny = ['bash(rm']",
                "permissions.deny: \"bash(rm\" has no closing parenthesis",
            ),
            (
                "deny = ['shell(rm)']",
                "permissions.deny: \"shell(rm)\" is neither a tool name",
            ),
            ("allow = ['']", "permissions.allow: an entry is empty"),
            ("allowExcept = [' ']", "permissions.allowExcept: an entry is empty"),
            ("autoApprove = ['ls']", "unknown field `autoApprove`"),
        ] {
            let error = format!("{:#}", parse(text).unwrap_err());
            assert!(error.contains(expected), "{text}: {error}");
        }
    }

    #[test]
    fn tool_names_are_checked_against_the_registered_tools() {
        let mcp = ["prom__query".to_string()];
        for (text, expected) in [
            ("ask = ['missing']", "permissions.ask: unknown tool \"missing\""),
            ("deny = ['missing']", "permissions.deny: unknown tool \"missing\""),
            ("allow = ['missing']", "permissions.allow: unknown tool \"missing\""),
            (
                "allow = ['read_file']",
                "permissions.allow: read_file runs without asking unless it is in permissions.ask",
            ),
            (
                "ask = ['prom__query']\nallow = ['prom__query']",
                "prom__query is in both permissions.ask and permissions.allow",
            ),
        ] {
            let error = format!("{:#}", parse(text).unwrap().validate(known, &mcp).unwrap_err());
            assert!(error.contains(expected), "{text}: {error}");
        }
    }
}
