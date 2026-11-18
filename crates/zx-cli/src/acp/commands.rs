//! The agent's slash commands: offered with `available_commands_update` once the client
//! knows the session, and run instead of a prompt when a prompt starts with one: `/name`,
//! which names the session, and `/compact`, which summarizes the old part of the
//! conversation as a turn of its own.

use agent_client_protocol::Client;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate, PromptResponse, SessionId, SessionInfoUpdate,
    SessionUpdate, StopReason, UnstructuredCommandInput,
};
use anyhow::Result;

use zx_agent::Session;
use zx_agent::transcript::SessionName;

use super::updates;
use crate::listing::preview_text;

/// The command that names the session, without its slash.
const NAME: &str = "name";
/// The command that summarizes the old part of the conversation, without its slash.
const COMPACT: &str = "compact";

/// A prompt that is one of the agent's commands.
pub(super) enum Command {
    /// `/name <name>`: name the session.
    Name(SessionName),
    /// `/compact`: summarize the old part of the conversation.
    Compact,
    /// A command without what it needs, or with what it does not take, and how to use it.
    Usage(&'static str),
}

/// The command `text` runs, if it starts with one: its first word, slash included, names
/// the command and the rest is the command's input. Any other text, slashes and all, is a
/// prompt for the model.
pub(super) fn parse(text: &str) -> Option<Command> {
    let text = text.trim();
    let (word, input) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    match word.strip_prefix('/')? {
        NAME => Some(match SessionName::new(input) {
            Some(name) => Command::Name(name),
            None => Command::Usage("give the session a name: /name <name>"),
        }),
        COMPACT if input.trim().is_empty() => Some(Command::Compact),
        COMPACT => Some(Command::Usage("/compact takes no input")),
        _ => None,
    }
}

/// Tell the client which commands the session offers.
pub(super) fn offer(cx: &ConnectionTo<Client>, session_id: &SessionId) {
    let name = AvailableCommand::new(
        NAME,
        "Name this session; lists show the name instead of the first prompt",
    )
    .input(AvailableCommandInput::Unstructured(UnstructuredCommandInput::new(
        "a name for this session",
    )));
    let compact = AvailableCommand::new(
        COMPACT,
        "Summarize the earlier part of the conversation, which makes room in the model's context",
    );
    updates::send(
        cx,
        session_id,
        SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(vec![name, compact])),
    );
}

/// Name `session`. The turn it answers ends at once, without a model request.
pub(super) async fn name(
    session: &Session,
    name: SessionName,
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
) -> Result<PromptResponse> {
    // The title `session/list` gives the session from now on.
    let title = preview_text(name.as_str());
    session.set_name(name).await?;
    updates::send(
        cx,
        session_id,
        SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new().title(title)),
    );
    Ok(PromptResponse::new(StopReason::EndTurn))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(text: &str) -> Option<String> {
        match parse(text)? {
            Command::Name(name) => Some(name.as_str().to_string()),
            Command::Compact => Some("compact".to_string()),
            Command::Usage(usage) => Some(format!("usage: {usage}")),
        }
    }

    #[test]
    fn a_prompt_that_starts_with_the_name_command_names_the_session() {
        assert_eq!(named("/name 排查 nginx 502").as_deref(), Some("排查 nginx 502"));
        assert_eq!(named("  /name\tfirst\nsecond  ").as_deref(), Some("first second"));
        for without in ["/name", "/name  \n "] {
            let answer = named(without).unwrap();
            assert!(
                answer.starts_with("usage: ") && answer.ends_with("/name <name>"),
                "{answer}"
            );
        }
    }

    #[test]
    fn the_compact_command_takes_no_input() {
        assert!(matches!(parse(" /compact \n"), Some(Command::Compact)));
        assert_eq!(
            named("/compact the first half").as_deref(),
            Some("usage: /compact takes no input")
        );
    }

    #[test]
    fn other_text_is_a_prompt_even_with_a_slash() {
        for prompt in [
            "/named things",
            "/NAME x",
            "name x",
            "//name x",
            "rename with /name x",
            "/compacted",
            "",
        ] {
            assert!(parse(prompt).is_none(), "{prompt:?}");
        }
    }
}
