//! What the tests of session files share: a header, an answer, and the texts a session
//! projects.

use std::path::Path;

use crate::ai::{AssistantMessage, ContentBlock, Message, StopReason};
use zx_core::transcript::{SessionHeader, Transcript};

pub(super) fn header() -> SessionHeader {
    SessionHeader::new(Path::new("/tmp"), "m")
}

pub(super) fn assistant(text: &str) -> Message {
    let mut message = AssistantMessage::failure("p", "m", StopReason::Stop, "");
    message.error_message = None;
    message.content = vec![ContentBlock::Text { text: text.into() }];
    Message::Assistant(message)
}

pub(super) fn texts(store: &Transcript) -> Vec<String> {
    store
        .messages()
        .iter()
        .map(|message| match message {
            Message::System(system) => format!("system:{}", system.sections.len()),
            Message::User { content, .. } => content.clone(),
            Message::Assistant(answer) => answer.text(),
            Message::ToolResult(result) => result.content.clone(),
        })
        .collect()
}
