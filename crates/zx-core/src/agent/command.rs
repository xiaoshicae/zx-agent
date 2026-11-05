//! Slash commands that extensions register (pi: `registerCommand`), run by a frontend when
//! the person types `/name args`.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use super::queue::{MessageQueues, QueuedMessage};
use super::tool::ActiveTools;

/// A command an extension adds to a frontend's slash commands. The frontend matches its own
/// commands first, so a name it already uses is never reached.
#[async_trait]
pub trait SlashCommand: Send + Sync {
    /// The name after the slash: ASCII letters, digits, `_` and `-`, starting with a letter.
    fn name(&self) -> &str;

    /// One line for a help listing.
    fn description(&self) -> &str;

    /// Run with the text after the name, empty when there is none, also while a run is in
    /// progress (pi runs extension commands at once). An error is shown to the person; it
    /// does not stop a run.
    async fn run(&self, args: &str, ctx: &CommandContext) -> Result<CommandOutcome, String>;
}

/// What a command knows about the conversation it runs in. Fields are only ever added, so
/// the struct is `#[non_exhaustive]`; build one with [`CommandContext::new`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct CommandContext {
    /// The tools' working directory.
    pub cwd: PathBuf,
    /// The session's ID.
    pub session_id: Option<String>,
    /// The conversation's queues: a command sends the model a message with `steer_message`
    /// or `follow_up_message`, and records state with `append_entry`.
    pub queues: Arc<MessageQueues>,
    /// The tools the next request offers.
    pub active_tools: Arc<ActiveTools>,
}

impl CommandContext {
    pub fn new(
        cwd: PathBuf,
        session_id: Option<String>,
        queues: Arc<MessageQueues>,
        active_tools: Arc<ActiveTools>,
    ) -> Self {
        Self {
            cwd,
            session_id,
            queues,
            active_tools,
        }
    }
}

/// What a command leaves for the frontend to do.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CommandOutcome {
    /// Lines to show the person.
    pub lines: Vec<String>,
    /// A message to send to the model now, as a prompt starts a run (pi: `sendMessage`
    /// with `triggerTurn`, or `sendUserMessage`). While a run is in progress the frontend
    /// queues it instead.
    pub prompt: Option<QueuedMessage>,
}

/// Why [`crate::AgentSession::run_command`] ran no command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandError {
    /// No extension registered a command with that name.
    Unknown,
    /// The command ran and failed with this message.
    Failed(String),
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => f.write_str("unknown command"),
            Self::Failed(reason) => write!(f, "command failed: {reason}"),
        }
    }
}

impl std::error::Error for CommandError {}

/// The rule a command name follows: ASCII letters, digits, `_` and `-`, starting with a
/// letter, so that `/name args` splits at the first space and the name cannot be mistaken
/// for a path.
pub fn check_command_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let well_formed = chars.next().is_some_and(|first| first.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if well_formed {
        Ok(())
    } else {
        Err(format!(
            "command name {name:?} must be ASCII letters, digits, _ and -, starting with a letter"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_names_are_plain_words() {
        for name in ["jira", "note-2", "run_all", "a"] {
            assert!(check_command_name(name).is_ok(), "{name}");
        }
        for name in ["", "1st", "/jira", "my notes", "etc/hosts", "名字"] {
            assert!(check_command_name(name).is_err(), "{name}");
        }
    }
}
