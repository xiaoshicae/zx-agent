//! What the unit tests of the transcript's modules share: a header, messages, the texts a
//! transcript projects, a compaction's plan and summary, a log that writes to a writer, and a writer
//! that always fails. The rest of the crate's tests reach [`FailingWriter`] and [`WriterLog`]
//! as `crate::transcript::...`.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;

use super::{
    CompactionPlan, CompactionSummary, LoadedSession, LogRecord, SessionHeader, SessionLog, SummarizedBy, SummaryFact,
    SummaryRequest, Transcript,
};
use crate::ai::{AssistantMessage, ContentBlock, Message, StopReason};

/// A log that writes each record and its newline to a writer, on a blocking thread as a file
/// would be written: a writer that fails or blocks makes the append fail or time out. An
/// append whose future is dropped keeps the writer, so later appends fail.
pub(crate) struct WriterLog {
    writer: Option<Box<dyn Write + Send + Sync>>,
}

impl WriterLog {
    pub(crate) fn new(writer: Box<dyn Write + Send + Sync>) -> Self {
        Self { writer: Some(writer) }
    }
}

#[async_trait]
impl SessionLog for WriterLog {
    async fn append(&mut self, record: &LogRecord<'_>) -> Result<()> {
        let mut writer = self.writer.take().context("an earlier write did not finish")?;
        let line = format!("{}\n", record.json);
        let (writer, written) = tokio::task::spawn_blocking(move || {
            let written = writer.write_all(line.as_bytes()).and_then(|()| writer.flush());
            (writer, written)
        })
        .await?;
        self.writer = Some(writer);
        Ok(written?)
    }
}

/// The session `header` with `records`, each a record's JSON, as a storage would read it
/// back, for loading or a snapshot.
pub(super) fn loaded(header: SessionHeader, records: Vec<String>) -> LoadedSession {
    LoadedSession {
        header,
        records,
        incomplete_tail_bytes: 0,
    }
}

/// The records of `transcript`, each as its log was given it.
pub(super) fn records_of(transcript: &Transcript) -> Vec<String> {
    transcript
        .entries()
        .iter()
        .map(|entry| serde_json::to_string(entry).expect("an entry serializes"))
        .collect()
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
        images: Vec::new(),
        is_error: false,
        timestamp: 1,
        duration_ms: None,
        details,
        usage: None,
    })
}

pub(super) fn header() -> SessionHeader {
    SessionHeader::new(Path::new("/tmp"))
}

pub(super) fn assistant(text: &str) -> Message {
    let message = AssistantMessage::new(
        "p",
        "m",
        StopReason::Stop,
        vec![ContentBlock::Text { text: text.into() }],
    );
    Message::Assistant(message)
}

/// An old message and the latest input, appended to `transcript`; returns their entry IDs.
pub(super) async fn seeded(transcript: &mut Transcript) -> (String, String) {
    let old = transcript.append_message(Message::user("old evidence")).await.unwrap();
    let kept = transcript.append_message(Message::user("latest input")).await.unwrap();
    (old, kept)
}

/// A summary whose one fact cites the record `id`.
pub(super) fn summary(id: &str) -> CompactionSummary {
    CompactionSummary {
        facts: vec![SummaryFact {
            text: "The old observation".into(),
            source_entry_ids: vec![id.into()],
        }],
        inferences: vec!["An inference".into()],
        plans: vec!["Continue the task".into()],
    }
}

/// A compaction of `transcript` that the model summarizes, keeping from `first_kept` and reading
/// every record before it.
pub(super) fn plan(transcript: &Transcript, first_kept: &str) -> CompactionPlan {
    let source_entry_ids = transcript
        .raw_projection()
        .into_iter()
        .take_while(|item| item.entry_id != first_kept)
        .map(|item| item.entry_id)
        .collect();
    CompactionPlan {
        first_kept_entry_id: first_kept.into(),
        kept_input_entry_id: None,
        source_entry_ids,
        by: SummarizedBy::Request(SummaryRequest {
            instructions: "Summarize".into(),
        }),
        input_bytes: 1000,
    }
}

pub(super) fn texts(transcript: &Transcript) -> Vec<String> {
    transcript
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
