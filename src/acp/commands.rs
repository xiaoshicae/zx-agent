//! The agent's slash commands: offered with `available_commands_update` once the client
//! knows the session, and run instead of a turn when a prompt starts with one. There is one,
//! `/name`, which names the session.

use agent_client_protocol::Client;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate, PromptResponse, SessionId, SessionInfoUpdate,
    SessionUpdate, StopReason, UnstructuredCommandInput,
};
use agent_client_protocol::{Error, ErrorCode};
use anyhow::Result;

use zx_agent::AgentSession;
use zx_agent::session::SessionName;

use super::updates;
use crate::session_files::preview_text;

/// The command that names the session, without its slash.
const NAME: &str = "name";

/// A prompt that is one of the agent's commands.
pub(super) enum Command {
    /// `/name <name>`: name the session.
    Name(SessionName),
    /// A command without what it needs, and how to use it.
    Usage(&'static str),
}

/// The command `text` runs, if it starts with one: its first word, slash included, names
/// the command and the rest is the command's input. Any other text, slashes and all, is a
/// prompt for the model.
pub(super) fn parse(text: &str) -> Option<Command> {
    let text = text.trim();
    let (word, input) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    if word.strip_prefix('/') != Some(NAME) {
        return None;
    }
    Some(match SessionName::new(input) {
        Some(name) => Command::Name(name),
        None => Command::Usage("give the session a name: /name <name>"),
    })
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
    updates::send(
        cx,
        session_id,
        SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(vec![name])),
    );
}

/// Run `command` on `session`. The turn it answers ends at once, without a model request.
pub(super) async fn run(
    session: &AgentSession,
    command: Command,
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
) -> Result<PromptResponse> {
    match command {
        Command::Name(name) => {
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
        Command::Usage(usage) => Err(Error::new(ErrorCode::INVALID_PARAMS, usage).into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(text: &str) -> Option<String> {
        match parse(text)? {
            Command::Name(name) => Some(name.as_str().to_string()),
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
    fn other_text_is_a_prompt_even_with_a_slash() {
        for prompt in [
            "/named things",
            "/NAME x",
            "name x",
            "//name x",
            "rename with /name x",
            "",
        ] {
            assert!(parse(prompt).is_none(), "{prompt:?}");
        }
    }
}
