//! What the unit tests of the session modules share: a header, messages, the texts a store
//! projects, a session in one file, and a writer that always fails. The rest of the crate's
//! tests reach [`FailingWriter`] as `crate::transcript::FailingWriter`.

use std::path::Path;

use anyhow::{Context, Result};

use crate::ai::{AssistantMessage, ContentBlock, Message, StopReason};

use super::{JsonlStorage, SessionHeader, SessionStorage, Transcript};

/// A new session with `header` in the file `path`.
pub(super) async fn create_file(path: &Path, header: SessionHeader) -> Result<Transcript> {
    let log = JsonlStorage::file(path).create(&header).await?;
    Ok(Transcript::with_log(header, log))
}

/// The session in the file `path`, loaded and open for appending.
pub(super) async fn open_file(path: &Path) -> Result<Transcript> {
    let (loaded, log) = JsonlStorage::file(path).open(&id_in(path)?).await?;
    Transcript::load(loaded, log)
}

/// A snapshot of the session in the file `path`.
pub(super) async fn read_file(path: &Path) -> Result<Transcript> {
    Transcript::snapshot(JsonlStorage::file(path).read(&id_in(path)?).await?)
}

/// The session ID in the header of the file `path`, whatever else the file holds.
fn id_in(path: &Path) -> Result<String> {
    let text = std::fs::read_to_string(path)?;
    let header: serde_json::Value = serde_json::from_str(text.lines().next().context("an empty file")?)?;
    Ok(header["id"].as_str().context("a header without an ID")?.to_string())
}

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

pub(super) fn texts(store: &Transcript) -> Vec<String> {
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
