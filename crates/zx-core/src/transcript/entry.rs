//! The records of a session: the header, the entries of the tree, and what each contributes
//! to the model's context.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use super::{
    CompactionEntry, CompactionFailedEntry, CompactionStartedEntry, RequestConfigEntry, RequestStartedEntry,
    SESSION_VERSION, SessionInfoEntry, ToolCallStartedEntry,
};
use crate::ai::{Message, UserMessageKind, now_ms};

/// What a session starts with, and never changes: when and where it started, and what its
/// host says about it. A storage keeps it before the records, as the first line of a session
/// file.
///
/// It names no model: a session's model can change between requests, and each request's is
/// in the request configuration recorded for it
/// ([`RequestConfig::model`](super::RequestConfig::model)).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
    /// The session format, [`SESSION_VERSION`]; a session of any other version is refused.
    pub version: u32,
    pub id: String,
    pub timestamp: u64,
    pub cwd: String,
    /// The ID of the session this one was forked from; absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
    /// What the host says about the session, such as its tenant or the chat it belongs to.
    /// The kernel only keeps them: a listing can ask for the sessions with given labels
    /// ([`SessionQuery`](super::SessionQuery)).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

impl SessionHeader {
    /// A header for a new session in `cwd`, with a fresh id and no labels.
    pub fn new(cwd: &Path) -> Self {
        Self {
            version: SESSION_VERSION,
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: now_ms(),
            cwd: cwd.display().to_string(),
            parent_session: None,
            labels: BTreeMap::new(),
        }
    }

    /// Fails for a session of another format than [`SESSION_VERSION`], which is neither read
    /// nor written: a storage refuses to create one, and loading refuses to read one.
    pub fn check_version(&self) -> Result<()> {
        ensure!(
            self.version == SESSION_VERSION,
            "session format version {} is not supported; only version {SESSION_VERSION} can be read or written",
            self.version
        );
        Ok(())
    }
}

/// A conversation message stored in the tree.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: u64,
    pub message: Message,
    /// Who sent the message, when it is a person's input and the host named the person
    /// ([`Input::User`](crate::input::Input::User)); absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
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

/// What an extension calls the records it appends (pi: `customType`): the name of the
/// extension or of the kind of record, such as `jira_issue`. Kept as written, and never blank:
/// a blank one would name nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CustomType(String);

impl CustomType {
    /// `text` as a custom type. `None` when it is empty or only whitespace.
    pub fn new(text: impl Into<String>) -> Option<Self> {
        let text = text.into();
        (!text.trim().is_empty()).then_some(Self(text))
    }

    /// The type as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CustomType {
    type Error = &'static str;

    // A stored type must be one that `new` could have made, so a loaded session holds no
    // custom record that could not have been appended.
    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::new(text).ok_or("a custom record needs a customType that is not blank")
    }
}

impl From<CustomType> for String {
    fn from(custom_type: CustomType) -> Self {
        custom_type.0
    }
}

/// A message an extension sends to the model (pi: `CustomMessage`). The model sees its
/// `content` as a user message; `custom_type`, `display` and `details` are for frontends
/// and for the extension itself.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMessage {
    pub custom_type: CustomType,
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
    pub custom_type: CustomType,
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
            images: Vec::new(),
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
            Self::CompactionStarted(entry) => &entry.id,
            Self::Compaction(entry) => &entry.id,
            Self::CompactionFailed(entry) => &entry.id,
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
            Self::CompactionStarted(entry) => entry.parent_id.as_deref(),
            Self::Compaction(entry) => entry.parent_id.as_deref(),
            Self::CompactionFailed(entry) => entry.parent_id.as_deref(),
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
            Self::CompactionStarted(entry) => entry.timestamp,
            Self::Compaction(entry) => entry.timestamp,
            Self::CompactionFailed(entry) => entry.timestamp,
            Self::RequestConfig(entry) => entry.timestamp,
            Self::RequestStarted(entry) => entry.timestamp,
            Self::ToolCallStarted(entry) => entry.timestamp,
            Self::Custom(entry) => entry.timestamp,
            Self::CustomMessage(entry) => entry.timestamp,
            Self::SessionInfo(entry) => entry.timestamp,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::transcript::Transcript;
    use crate::transcript::test_support::{header, loaded};

    #[test]
    fn a_header_records_its_format_its_start_and_its_directory_and_no_model() {
        let header = SessionHeader {
            id: "h".into(),
            timestamp: 1,
            ..SessionHeader::new(Path::new("/w"))
        };

        assert_eq!(
            serde_json::to_string(&header).unwrap(),
            r#"{"version":16,"id":"h","timestamp":1,"cwd":"/w"}"#
        );
    }

    #[test]
    fn a_header_of_version_15_is_refused_as_a_session_of_any_other_version_is() {
        // As version 15 wrote it, with the model the session started with.
        let stored = json!({"version": 15, "id": "h", "timestamp": 1, "cwd": "/w", "model": "m"});
        let header: SessionHeader = serde_json::from_value(stored).unwrap();

        let error = Transcript::snapshot(loaded(header, Vec::new())).err().unwrap();

        assert_eq!(
            error.to_string(),
            "session format version 15 is not supported; only version 16 can be read or written"
        );
    }

    #[test]
    fn a_custom_type_is_kept_as_written_and_is_never_blank() {
        let padded = CustomType::new(" jira issue ").unwrap();
        assert_eq!(padded.as_str(), " jira issue ");
        assert_eq!(serde_json::to_value(&padded).unwrap(), json!(" jira issue "));
        for blank in ["", " ", "\n\t", "\u{3000}"] {
            assert_eq!(CustomType::new(blank), None, "{blank:?}");
        }
    }

    #[test]
    fn opening_refuses_a_custom_record_whose_type_is_blank() {
        for record in [
            json!({"type": "custom", "id": "c1", "parentId": null, "timestamp": 2, "customType": " "}),
            json!({"type": "custom_message", "id": "c1", "parentId": null, "timestamp": 2,
                "customType": "", "content": "x", "display": true}),
        ] {
            let error = Transcript::snapshot(loaded(header(), vec![record.to_string()]))
                .err()
                .unwrap_or_else(|| panic!("{record} was accepted"));
            assert!(
                format!("{error:#}").contains("a custom record needs a customType"),
                "{record}: {error:#}"
            );
        }
    }
}
