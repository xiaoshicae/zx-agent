//! What the unit tests of the session modules share: a header, messages, the texts a store
//! projects, and a writer that always fails. The rest of the crate's tests reach
//! [`FailingWriter`] as `crate::session::FailingWriter`.

use std::path::Path;

use crate::ai::{AssistantMessage, ContentBlock, Message, StopReason};

use super::{SessionHeader, SessionStore};

/// A writer whose every write fails, like a full disk.
pub(crate) struct FailingWriter;

impl std::io::Write for FailingWriter {
    fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("disk full"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) fn result_with_details(details: Option<serde_json::Value>) -> Message {
    Message::ToolResult(crate::ai::ToolResultMessage {
        tool_call_id: "c1".into(),
        tool_name: "bash".into(),
        content: "visible".into(),
        is_error: false,
        timestamp: 1,
        duration_ms: None,
        details,
        usage: None,
    })
}

pub(super) fn header() -> SessionHeader {
    SessionHeader::new(Path::new("/tmp"), "m")
}

pub(super) fn assistant(text: &str) -> Message {
    let mut message = AssistantMessage::failure("p", "m", StopReason::Stop, "");
    message.error_message = None;
    message.content = vec![ContentBlock::Text { text: text.into() }];
    Message::Assistant(message)
}

pub(super) fn texts(store: &SessionStore) -> Vec<String> {
    store
        .messages()
        .iter()
        .map(|m| match m {
            Message::System(system) => format!("system:{}", system.sections.len()),
            Message::User { content, .. } => content.clone(),
            Message::Assistant(a) => a.text(),
            Message::ToolResult(r) => r.content.clone(),
        })
        .collect()
}
