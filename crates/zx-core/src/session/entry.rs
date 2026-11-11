//! The records of a session: the header, the entries of the tree, and what each contributes
//! to the model's context.

use std::borrow::Cow;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{
    CompactionEntry, CompactionFailedEntry, CompactionStartedEntry, RequestConfigEntry, RequestStartedEntry,
    SESSION_VERSION, SessionInfoEntry, ToolCallStartedEntry,
};
use crate::ai::{Message, UserMessageKind, now_ms};

/// First line of a session file. Records where and with which model the session started.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
    /// The session format, [`SESSION_VERSION`]; a session of any other version is refused.
    pub version: u32,
    pub id: String,
    pub timestamp: u64,
    pub cwd: String,
    pub model: String,
    /// The session file this one was forked from; absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
}

impl SessionHeader {
    /// A header for a new session with a fresh id.
    pub fn new(cwd: &Path, model: &str) -> Self {
        Self {
            version: SESSION_VERSION,
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: now_ms(),
            cwd: cwd.display().to_string(),
            model: model.to_string(),
            parent_session: None,
        }
    }
}

/// The header as the first line of a session file: tagged `session`, like an entry is tagged
/// with its type.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum HeaderLine {
    Session(SessionHeader),
}

/// A conversation message stored in the tree.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: u64,
    pub message: Message,
}

/// Changes what one earlier message contributes to model context, on this branch only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextEditEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: u64,
    pub target_id: String,
    /// `None` omits the target from model context; `Some` replaces its text content.
    pub replacement: Option<String>,
}

/// A message an extension sends to the model (pi: `CustomMessage`). The model sees its
/// `content` as a user message; `custom_type`, `display` and `details` are for frontends
/// and for the extension itself.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMessage {
    /// Names the extension or the kind of message, such as `jira_issue`; not blank.
    pub custom_type: String,
    pub content: String,
    /// Whether a frontend shows the message to the person; it reaches the model either way.
    pub display: bool,
    /// Data for the extension, never sent to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

/// State an extension keeps in the session (pi: `appendEntry`). Never in model context.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: u64,
    pub custom_type: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub data: serde_json::Value,
}

/// A [`CustomMessage`] in the tree: in model context as a user message, like a stored
/// message, and a target for context edits.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMessageEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: u64,
    #[serde(flatten)]
    pub message: CustomMessage,
}

impl CustomMessageEntry {
    /// The message as the model sees it.
    pub(crate) fn projected(&self) -> Message {
        Message::User {
            content: self.message.content.clone(),
            timestamp: self.timestamp,
            kind: UserMessageKind::Custom,
        }
    }
}

/// One record of a session after the header: one line of a session file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Entry {
    Message(MessageEntry),
    ContextEdit(ContextEditEntry),
    CompactionStarted(CompactionStartedEntry),
    Compaction(CompactionEntry),
    CompactionFailed(CompactionFailedEntry),
    RequestConfig(RequestConfigEntry),
    RequestStarted(RequestStartedEntry),
    ToolCallStarted(ToolCallStartedEntry),
    Custom(CustomEntry),
    CustomMessage(CustomMessageEntry),
    SessionInfo(SessionInfoEntry),
}

impl Entry {
    /// The entry's id.
    pub fn id(&self) -> &str {
        match self {
            Self::Message(entry) => &entry.id,
            Self::ContextEdit(entry) => &entry.id,
            Self::CompactionFailed(entry) => &entry.id,
            Self::Compaction(entry) => &entry.id,
            Self::CompactionStarted(entry) => &entry.id,
            Self::RequestConfig(entry) => &entry.id,
            Self::RequestStarted(entry) => &entry.id,
            Self::ToolCallStarted(entry) => &entry.id,
            Self::Custom(entry) => &entry.id,
            Self::CustomMessage(entry) => &entry.id,
            Self::SessionInfo(entry) => &entry.id,
        }
    }

    /// The parent entry's id; `None` for a root entry.
    pub fn parent_id(&self) -> Option<&str> {
        match self {
            Self::Message(entry) => entry.parent_id.as_deref(),
            Self::ContextEdit(entry) => entry.parent_id.as_deref(),
            Self::CompactionFailed(entry) => entry.parent_id.as_deref(),
            Self::Compaction(entry) => entry.parent_id.as_deref(),
            Self::CompactionStarted(entry) => entry.parent_id.as_deref(),
            Self::RequestConfig(entry) => entry.parent_id.as_deref(),
            Self::RequestStarted(entry) => entry.parent_id.as_deref(),
            Self::ToolCallStarted(entry) => entry.parent_id.as_deref(),
            Self::Custom(entry) => entry.parent_id.as_deref(),
            Self::CustomMessage(entry) => entry.parent_id.as_deref(),
            Self::SessionInfo(entry) => entry.parent_id.as_deref(),
        }
    }

    /// The serialized `type`, such as `context_edit`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Message(_) => "message",
            Self::ContextEdit(_) => "context_edit",
            Self::CompactionStarted(_) => "compaction_started",
            Self::Compaction(_) => "compaction",
            Self::CompactionFailed(_) => "compaction_failed",
            Self::RequestConfig(_) => "request_config",
            Self::RequestStarted(_) => "request_started",
            Self::ToolCallStarted(_) => "tool_call_started",
            Self::Custom(_) => "custom",
            Self::CustomMessage(_) => "custom_message",
            Self::SessionInfo(_) => "session_info",
        }
    }

    /// The message the entry contributes to the model's context, before context edits: a
    /// stored message as it is, an extension's message as a user message. `None` for the
    /// records that are not messages.
    pub(crate) fn projected(&self) -> Option<Cow<'_, Message>> {
        match self {
            Self::Message(entry) => Some(Cow::Borrowed(&entry.message)),
            Self::CustomMessage(entry) => Some(Cow::Owned(entry.projected())),
            Self::ContextEdit(_)
            | Self::CompactionStarted(_)
            | Self::Compaction(_)
            | Self::CompactionFailed(_)
            | Self::RequestConfig(_)
            | Self::RequestStarted(_)
            | Self::ToolCallStarted(_)
            | Self::Custom(_)
            | Self::SessionInfo(_) => None,
        }
    }

    /// When the entry was appended, in milliseconds since the Unix epoch.
    pub fn timestamp(&self) -> u64 {
        match self {
            Self::Message(entry) => entry.timestamp,
            Self::ContextEdit(entry) => entry.timestamp,
            Self::CompactionFailed(entry) => entry.timestamp,
            Self::Compaction(entry) => entry.timestamp,
            Self::CompactionStarted(entry) => entry.timestamp,
            Self::RequestConfig(entry) => entry.timestamp,
            Self::RequestStarted(entry) => entry.timestamp,
            Self::ToolCallStarted(entry) => entry.timestamp,
            Self::Custom(entry) => entry.timestamp,
            Self::CustomMessage(entry) => entry.timestamp,
            Self::SessionInfo(entry) => entry.timestamp,
        }
    }
}
