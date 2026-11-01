//! Append-only session tree and its projection (pi: coding-agent SessionManager).
//!
//! Every change appends an entry; stored entries are never rewritten. Entries link to their
//! parent, so the session is a tree and the "leaf" pointer selects the current branch. The
//! messages a model sees are a projection of that branch: context edits can omit or replace
//! earlier messages without touching the raw history, which stays available for auditing.
//!
//! A [`SessionLog`] keeps the entries: the JSONL session file, or a backend of the host's
//! choice. The file's one exception to append-only is `open`, which cuts off an unfinished
//! last line left by a crash or failed write, and reports how many bytes it removed.
//!
//! Version 3 adds auditable compaction without rewriting earlier records. Version 4 records
//! that a tool call with side effects started, so recovery can tell interrupted calls apart.

mod compaction;
mod file;
mod log;
mod recovery;
mod request;
pub use compaction::{
    CompactionEntry, CompactionFailedEntry, CompactionPlan, CompactionStartedEntry, CompactionSummary,
    FormatUpgradeEntry, SummaryFact, SummaryRequest,
};
pub use log::{LoadedSession, LogRecord, SessionLog};
pub use recovery::{InterruptedToolCall, StartRecord, ToolCallStartedEntry};
pub use request::{RequestConfig, RequestConfigEntry, RequestSource, RequestStartedEntry};

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};

#[cfg(test)]
use crate::ai::ContentBlock;
use crate::ai::{Message, now_ms};
use file::JsonlFile;

/// Format version written to new session headers. Version 4 adds `tool_call_started`.
/// Version 3 adds auditable compaction, request records and append-only format upgrades.
/// Version 2 records the system prompt as
/// system messages in the conversation; version 1 kept it in the header. Version 1 files still
/// open: the old header field is ignored, and the next run records the prompt as a message.
pub const SESSION_VERSION: u32 = 4;

/// How long one append may wait for its log, such as a database that stopped answering.
/// A file append finishes long before; a timed-out append stops writing, like a failed one.
const APPEND_TIMEOUT: Duration = Duration::from_secs(30);

/// Why a store no longer writes. Each case asks for something different before a session can
/// be written again, so hosts such as a server that keeps sessions open can match on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WritesStopped {
    /// An append was dropped while it waited for its log, so the log may or may not hold the
    /// record. Open the session again from its storage before writing.
    Interrupted,
    /// An append did not finish within this time; its outcome is unknown, as above.
    TimedOut(Duration),
    /// An append failed with this error. Remove the cause, then open the session again.
    Failed(String),
    /// The store is a snapshot from [`SessionStore::read`], which never writes.
    ReadOnly,
}

impl fmt::Display for WritesStopped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Interrupted => {
                f.write_str("an earlier append was interrupted before its log answered, so its outcome is unknown")
            }
            Self::TimedOut(timeout) => write!(
                f,
                "an earlier append did not finish within {timeout:?}, so its outcome is unknown"
            ),
            Self::Failed(error) => write!(f, "an earlier write failed: {error}"),
            Self::ReadOnly => f.write_str("it was read as a snapshot, which is read-only"),
        }
    }
}

/// First line of a session file. Records where and with which model the session started.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
    pub version: u32,
    pub id: String,
    pub timestamp: u64,
    pub cwd: String,
    pub model: String,
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
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum HeaderLine {
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextEditEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: u64,
    pub target_id: String,
    /// `None` omits the target from model context; `Some` replaces its text content.
    pub replacement: Option<String>,
}

/// One record of a session after the header: one line of a session file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Entry {
    Message(MessageEntry),
    ContextEdit(ContextEditEntry),
    FormatUpgrade(FormatUpgradeEntry),
    CompactionStarted(CompactionStartedEntry),
    Compaction(CompactionEntry),
    CompactionFailed(CompactionFailedEntry),
    RequestConfig(RequestConfigEntry),
    RequestStarted(RequestStartedEntry),
    ToolCallStarted(ToolCallStartedEntry),
}

impl Entry {
    /// The entry's id.
    pub fn id(&self) -> &str {
        match self {
            Entry::Message(entry) => &entry.id,
            Entry::ContextEdit(entry) => &entry.id,
            Entry::CompactionFailed(entry) => &entry.id,
            Entry::Compaction(entry) => &entry.id,
            Entry::CompactionStarted(entry) => &entry.id,
            Entry::FormatUpgrade(entry) => &entry.id,
            Entry::RequestConfig(entry) => &entry.id,
            Entry::RequestStarted(entry) => &entry.id,
            Entry::ToolCallStarted(entry) => &entry.id,
        }
    }

    /// The parent entry's id; `None` for a root entry.
    pub fn parent_id(&self) -> Option<&str> {
        match self {
            Entry::Message(entry) => entry.parent_id.as_deref(),
            Entry::ContextEdit(entry) => entry.parent_id.as_deref(),
            Entry::CompactionFailed(entry) => entry.parent_id.as_deref(),
            Entry::Compaction(entry) => entry.parent_id.as_deref(),
            Entry::CompactionStarted(entry) => entry.parent_id.as_deref(),
            Entry::FormatUpgrade(entry) => entry.parent_id.as_deref(),
            Entry::RequestConfig(entry) => entry.parent_id.as_deref(),
            Entry::RequestStarted(entry) => entry.parent_id.as_deref(),
            Entry::ToolCallStarted(entry) => entry.parent_id.as_deref(),
        }
    }

    /// The serialized `type`, such as `context_edit`.
    pub fn kind(&self) -> &'static str {
        match self {
            Entry::Message(_) => "message",
            Entry::ContextEdit(_) => "context_edit",
            Entry::FormatUpgrade(_) => "format_upgrade",
            Entry::CompactionStarted(_) => "compaction_started",
            Entry::Compaction(_) => "compaction",
            Entry::CompactionFailed(_) => "compaction_failed",
            Entry::RequestConfig(_) => "request_config",
            Entry::RequestStarted(_) => "request_started",
            Entry::ToolCallStarted(_) => "tool_call_started",
        }
    }

    /// When the entry was appended, in milliseconds since the Unix epoch.
    pub fn timestamp(&self) -> u64 {
        match self {
            Entry::Message(entry) => entry.timestamp,
            Entry::ContextEdit(entry) => entry.timestamp,
            Entry::CompactionFailed(entry) => entry.timestamp,
            Entry::Compaction(entry) => entry.timestamp,
            Entry::CompactionStarted(entry) => entry.timestamp,
            Entry::FormatUpgrade(entry) => entry.timestamp,
            Entry::RequestConfig(entry) => entry.timestamp,
            Entry::RequestStarted(entry) => entry.timestamp,
            Entry::ToolCallStarted(entry) => entry.timestamp,
        }
    }
}

/// A model-visible message and the entry it came from.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectedMessage {
    pub entry_id: String,
    pub message: Message,
}

/// A session tree, kept in memory and optionally stored by a [`SessionLog`].
///
/// Methods that write are `async`: each waits until its log has stored the entry, and only
/// then changes memory. After a failed write, every later append fails without writing; so
/// does every append after one that timed out or whose future was dropped, because its
/// outcome is unknown. A failed write can leave a partial record; stopping there keeps it the
/// last one, which opening the session repairs. One store is the only writer of its session.
/// Appending to a log waits with a Tokio timer, so it needs a Tokio runtime with time enabled.
pub struct SessionStore {
    path: Option<PathBuf>,
    log: Option<Box<dyn SessionLog>>,
    /// Why this store no longer writes, if it does not.
    stopped: Option<WritesStopped>,
    append_timeout: Duration,
    header: SessionHeader,
    format_version: u32,
    entries: Vec<Entry>,
    index: HashMap<String, usize>,
    leaf: Option<String>,
    recovered_tail_bytes: usize,
}

impl SessionStore {
    /// A session kept only in memory.
    pub fn in_memory(header: SessionHeader) -> Self {
        Self {
            path: None,
            log: None,
            stopped: None,
            append_timeout: APPEND_TIMEOUT,
            format_version: header.version,
            header,
            entries: Vec::new(),
            index: HashMap::new(),
            leaf: None,
            recovered_tail_bytes: 0,
        }
    }

    /// A new session whose entries go to `log`. The backend has already stored `header`.
    pub fn with_log(header: SessionHeader, log: Box<dyn SessionLog>) -> Self {
        Self {
            log: Some(log),
            ..Self::in_memory(header)
        }
    }

    /// Rebuild a stored session from `loaded` and append to `log` from then on. Checks the
    /// format version and every record, as opening a session file does. The leaf is the last
    /// entry.
    pub fn load(loaded: LoadedSession, log: Box<dyn SessionLog>) -> Result<Self> {
        let mut store = Self::replay(loaded)?;
        store.log = Some(log);
        Ok(store)
    }

    fn replay(loaded: LoadedSession) -> Result<Self> {
        let LoadedSession {
            header,
            records,
            repaired_bytes,
        } = loaded;
        if header.version > SESSION_VERSION {
            bail!(
                "session version {} is newer than the supported version {SESSION_VERSION}",
                header.version
            );
        }
        let mut store = Self::in_memory(header);
        for (position, record) in (1..).zip(&records) {
            let entry: Entry = serde_json::from_str(record).map_err(|error| anyhow!("entry {position}: {error}"))?;
            store
                .validate_entry(&entry)
                .map_err(|error| anyhow!("entry {position}: {error:#}"))?;
            store.insert(entry);
        }
        store.recovered_tail_bytes = repaired_bytes;
        Ok(store)
    }

    /// Create a new session file. Fails if the file exists. Unlike pi, the header is written
    /// immediately, so even a run that crashes before the first answer leaves a record.
    /// On Unix the file stays locked while the store exists; see [`SessionStore::open`].
    pub fn create(path: &Path, header: SessionHeader) -> Result<Self> {
        let file = JsonlFile::create(path, &header)?;
        Ok(Self {
            path: Some(path.to_path_buf()),
            ..Self::with_log(header, Box::new(file))
        })
    }

    /// Open an existing session file for reading and appending. The leaf is the last entry.
    ///
    /// On Unix the file is locked with `flock(2)` while the store exists, so opening it again,
    /// in this process or another, fails until this store is dropped. A crash during the final
    /// write can leave an incomplete last line. Once every other record has been checked, that
    /// line is cut off so later appends start on a clean line; damage anywhere else is an
    /// error, and the file is left unchanged.
    pub fn open(path: &Path) -> Result<Self> {
        let (loaded, file) = JsonlFile::open(path)?;
        let mut store = Self::replay(loaded).map_err(|error| anyhow!("{}: {error:#}", path.display()))?;
        store.log = Some(Box::new(file.repair()?));
        store.path = Some(path.to_path_buf());
        Ok(store)
    }

    /// Read a session file without opening it for writing: a snapshot, also of a session
    /// that another store is writing. It neither takes the lock nor changes the file; an
    /// incomplete last line, such as a record still being written, is left out. Every append
    /// to the snapshot fails.
    pub fn read(path: &Path) -> Result<Self> {
        let loaded = file::read(path)?;
        let mut store = Self::replay(loaded).map_err(|error| anyhow!("{}: {error:#}", path.display()))?;
        store.path = Some(path.to_path_buf());
        store.stopped = Some(WritesStopped::ReadOnly);
        Ok(store)
    }

    /// Why this store no longer writes, or `None` while it does. Every append fails from
    /// then on; [`WritesStopped`] says what to do before writing the session again.
    pub fn writes_stopped(&self) -> Option<&WritesStopped> {
        self.stopped.as_ref()
    }

    /// A session that writes entries to `writer` instead of a file, so tests can make writes
    /// fail. The header is not written.
    #[cfg(test)]
    pub(crate) fn with_writer(header: SessionHeader, writer: Box<dyn std::io::Write + Send + Sync>) -> Self {
        Self::with_log(header, Box::new(JsonlFile::with_writer(writer)))
    }

    /// The header the session was created with.
    pub fn header(&self) -> &SessionHeader {
        &self.header
    }

    /// The session file; `None` for a session in memory or in another kind of log.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Bytes of an incomplete final record left out when the session was loaded, which
    /// `open` also removes from the file; zero if there was none. Complete corrupt records
    /// are errors and are never silently removed.
    pub fn recovered_tail_bytes(&self) -> usize {
        self.recovered_tail_bytes
    }

    /// Every entry in the order it was appended, on all branches.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The entry with `id`, on any branch.
    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.index.get(id).map(|&i| &self.entries[i])
    }

    /// The current position in the tree; `None` before the first entry.
    pub fn leaf_id(&self) -> Option<&str> {
        self.leaf.as_deref()
    }

    /// Append a message as a child of the leaf and make it the leaf. Returns its id.
    pub async fn append_message(&mut self, message: Message) -> Result<String> {
        let entry = Entry::Message(MessageEntry {
            id: self.new_id(),
            parent_id: self.leaf.clone(),
            timestamp: now_ms(),
            message,
        });
        self.append(entry).await
    }

    /// Omit (`None`) or replace the text of a message on the current branch, for future
    /// model context only. The target keeps its original content in the log.
    /// System and synthetic summary messages cannot be edited directly. Editing a
    /// summary's original evidence invalidates it and restores the original projection.
    pub async fn append_context_edit(&mut self, target_id: &str, replacement: Option<String>) -> Result<String> {
        let entry = Entry::ContextEdit(ContextEditEntry {
            id: self.new_id(),
            parent_id: self.leaf.clone(),
            timestamp: now_ms(),
            target_id: target_id.to_string(),
            replacement,
        });
        self.append(entry).await
    }

    /// The rules an edit follows, when it is appended and when a session is loaded: it
    /// targets an earlier message on its own branch, not a system message (prompt changes
    /// are new system messages), and a text replacement does not erase tool calls, whose
    /// results would otherwise lose their call.
    fn validate_context_edit(&self, edit: &ContextEditEntry) -> Result<()> {
        let target = edit.target_id.as_str();
        match self.get(target) {
            Some(Entry::Message(entry)) => match &entry.message {
                Message::System(_) => {
                    bail!("entry {target} is a system message; record prompt changes as new system messages")
                }
                Message::Assistant(answer) if edit.replacement.is_some() && !answer.tool_calls().is_empty() => {
                    bail!("entry {target} has tool calls, which a text replacement would erase")
                }
                Message::Assistant(_) | Message::User { .. } | Message::ToolResult(_) => {}
            },
            Some(
                Entry::ContextEdit(_)
                | Entry::FormatUpgrade(_)
                | Entry::CompactionStarted(_)
                | Entry::Compaction(_)
                | Entry::CompactionFailed(_)
                | Entry::RequestConfig(_)
                | Entry::RequestStarted(_)
                | Entry::ToolCallStarted(_),
            ) => bail!("entry {target} is not a message"),
            None => bail!("entry {target} not found"),
        }
        let mut ancestor = edit.parent_id.as_deref();
        while let Some(id) = ancestor {
            if id == target {
                return Ok(());
            }
            ancestor = self.get(id).and_then(Entry::parent_id);
        }
        bail!("entry {target} is not on the edit's branch")
    }

    /// Move the leaf to an existing entry. The next append starts a new branch there.
    /// Only the pointer moves; nothing is written until the next append.
    pub fn branch(&mut self, id: &str) -> Result<()> {
        if !self.index.contains_key(id) {
            bail!("entry {id} not found");
        }
        self.leaf = Some(id.to_string());
        Ok(())
    }

    /// Entries from the root to the leaf.
    pub fn branch_path(&self) -> Vec<&Entry> {
        self.branch_path_from(self.leaf.as_deref())
    }

    fn branch_path_from(&self, leaf: Option<&str>) -> Vec<&Entry> {
        let mut path = Vec::new();
        let mut current = leaf.and_then(|id| self.get(id));
        while let Some(entry) = current {
            path.push(entry);
            current = entry.parent_id().and_then(|id| self.get(id));
        }
        path.reverse();
        path
    }

    /// The model-visible messages of the current branch, with their source entries.
    pub fn projection(&self) -> Vec<ProjectedMessage> {
        self.compacted_projection()
    }

    fn raw_projection_from(path: &[&Entry]) -> Vec<ProjectedMessage> {
        // Later edits of the same target win.
        let mut edits: HashMap<&str, &Option<String>> = HashMap::new();
        for entry in path {
            if let Entry::ContextEdit(edit) = entry {
                edits.insert(edit.target_id.as_str(), &edit.replacement);
            }
        }
        path.iter()
            .filter_map(|entry| {
                let Entry::Message(entry) = entry else { return None };
                let message = match edits.get(entry.id.as_str()) {
                    None => entry.message.clone(),
                    Some(None) => return None,
                    Some(Some(text)) => entry.message.with_replaced_text(text),
                };
                Some(ProjectedMessage {
                    entry_id: entry.id.clone(),
                    message,
                })
            })
            .collect()
    }

    /// The model-visible messages of the current branch.
    pub fn messages(&self) -> Vec<Message> {
        self.projection()
            .into_iter()
            .map(|projected| projected.message)
            .collect()
    }

    async fn append(&mut self, entry: Entry) -> Result<String> {
        self.validate_entry(&entry)?;
        if let Some(reason) = &self.stopped {
            // Appending after a partial record would glue the next entry to it, turning a
            // repairable tail into a corrupt record in the middle of the session.
            bail!("the session is no longer written: {reason}");
        }
        if let Some(log) = &mut self.log {
            // Store before updating memory: an entry missing from the log must never become
            // the parent of later entries (pi: SessionManager._appendEntry).
            let json = serde_json::to_string(&entry)?;
            let record = LogRecord {
                seq: u64::try_from(self.entries.len())? + 1,
                id: entry.id(),
                parent_id: entry.parent_id(),
                kind: entry.kind(),
                timestamp: entry.timestamp(),
                json: &json,
            };
            // Until the log answers, the outcome is unknown. If this future is dropped while
            // waiting, the reason stays and nothing more is written.
            self.stopped = Some(WritesStopped::Interrupted);
            match tokio::time::timeout(self.append_timeout, log.append(&record)).await {
                Ok(Ok(())) => self.stopped = None,
                Ok(Err(error)) => {
                    self.stopped = Some(WritesStopped::Failed(format!("{error:#}")));
                    return Err(error.context("cannot write session entry"));
                }
                Err(_) => {
                    let timeout = self.append_timeout;
                    self.stopped = Some(WritesStopped::TimedOut(timeout));
                    bail!("cannot write session entry: the log did not answer within {timeout:?}");
                }
            }
        }
        let id = entry.id().to_string();
        self.insert(entry);
        Ok(id)
    }

    fn insert(&mut self, entry: Entry) {
        if let Entry::FormatUpgrade(upgrade) = &entry {
            self.format_version = upgrade.version;
        }
        let id = entry.id().to_string();
        self.index.insert(id.clone(), self.entries.len());
        self.entries.push(entry);
        self.leaf = Some(id);
    }

    /// Short id like pi's: 8 hex characters, collision-checked.
    fn new_id(&self) -> String {
        loop {
            let id = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
            if !self.index.contains_key(&id) {
                return id;
            }
        }
    }
}

/// A writer whose every write fails, like a full disk.
#[cfg(test)]
pub(crate) struct FailingWriter;

#[cfg(test)]
impl std::io::Write for FailingWriter {
    fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("disk full"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::sync::mpsc;

    use async_trait::async_trait;
    use futures::FutureExt;

    use super::*;
    use crate::ai::{AssistantMessage, StopReason, ToolCall, Usage};

    /// A session file with a v2 header followed by `entries`, one per line, unterminated.
    fn session_file(dir: &Path, entries: &[&str]) -> PathBuf {
        let path = dir.join("s.jsonl");
        let header = r#"{"type":"session","version":2,"id":"h","timestamp":1,"cwd":"/","model":"m"}"#;
        let lines: Vec<&str> = std::iter::once(header).chain(entries.iter().copied()).collect();
        std::fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    fn user_entry(id: &str, parent: Option<&str>) -> String {
        let parent = parent.map_or("null".to_string(), |p| format!("\"{p}\""));
        format!(
            r#"{{"type":"message","id":"{id}","parentId":{parent},"timestamp":1,"message":{{"role":"user","content":"{id}","timestamp":1}}}}"#
        )
    }

    fn header() -> SessionHeader {
        SessionHeader::new(Path::new("/tmp"), "m")
    }

    fn assistant(text: &str) -> Message {
        let mut message = AssistantMessage::failure("p", "m", StopReason::Stop, "");
        message.error_message = None;
        message.content = vec![ContentBlock::Text { text: text.into() }];
        Message::Assistant(message)
    }

    fn texts(store: &SessionStore) -> Vec<String> {
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

    /// Sends every record it stores to the test, which keeps the receiver.
    struct ChannelLog(mpsc::Sender<(u64, String)>);

    #[async_trait]
    impl SessionLog for ChannelLog {
        async fn append(&mut self, record: &LogRecord<'_>) -> Result<()> {
            self.0.send((record.seq, record.json.to_string()))?;
            Ok(())
        }
    }

    /// Never answers, like a database that stopped responding.
    struct Unresponsive;

    #[async_trait]
    impl SessionLog for Unresponsive {
        async fn append(&mut self, _record: &LogRecord<'_>) -> Result<()> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn a_text_replacement_cannot_erase_tool_calls() {
        let mut store = SessionStore::in_memory(header());
        store.append_message(Message::user("q")).await.unwrap();
        let mut answer = AssistantMessage::failure("p", "m", StopReason::ToolUse, "");
        answer.error_message = None;
        answer.content = vec![ContentBlock::ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({}),
            arguments_error: None,
        }];
        let target = store.append_message(Message::Assistant(answer)).await.unwrap();

        let error = store
            .append_context_edit(&target, Some("no calls".into()))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("tool calls"), "{error}");
        store.append_context_edit(&target, None).await.unwrap();
    }

    #[tokio::test]
    async fn opening_rejects_a_context_edit_of_a_system_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut store = SessionStore::create(&path, header()).unwrap();
        let system = store
            .append_message(Message::System(crate::ai::SystemMessage {
                sections: vec![crate::ai::SectionChange {
                    name: "preamble".into(),
                    content: Some("rules".into()),
                }],
                timestamp: 0,
            }))
            .await
            .unwrap();
        store.append_message(Message::user("q")).await.unwrap();
        let leaf = store.leaf.clone();
        drop(store);
        // Written by another program, or by hand: appending through the store refuses it.
        let edit = Entry::ContextEdit(ContextEditEntry {
            id: "edit".into(),
            parent_id: leaf,
            timestamp: 0,
            target_id: system,
            replacement: None,
        });
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{}", serde_json::to_string(&edit).unwrap()).unwrap();
        drop(file);

        let error = SessionStore::open(&path).err().expect("the edit must be refused");

        assert!(format!("{error:#}").contains("system message"), "{error:#}");
    }

    #[tokio::test]
    async fn context_edits_omit_or_replace_and_latest_wins() {
        let mut store = SessionStore::in_memory(header());
        store.append_message(Message::user("q")).await.unwrap();
        let failed = store.append_message(assistant("bad")).await.unwrap();
        let answer = store.append_message(assistant("good")).await.unwrap();
        store.append_context_edit(&failed, None).await.unwrap();
        assert_eq!(texts(&store), ["q", "good"]);
        store.append_context_edit(&answer, Some("first".into())).await.unwrap();
        store.append_context_edit(&answer, Some("second".into())).await.unwrap();
        assert_eq!(texts(&store), ["q", "second"]);
        assert_eq!(store.entries().len(), 6, "raw history keeps every entry");
    }

    #[tokio::test]
    async fn a_failed_write_leaves_the_tree_unchanged() {
        let mut store = SessionStore::with_writer(header(), Box::new(FailingWriter));
        assert!(store.append_message(Message::user("q")).await.is_err());
        assert!(store.entries().is_empty());
        assert_eq!(store.leaf_id(), None);
    }

    #[tokio::test]
    async fn a_log_receives_numbered_records_that_load_into_the_same_session() {
        let (sender, records) = mpsc::channel();
        let mut store = SessionStore::with_log(header(), Box::new(ChannelLog(sender)));
        let question = store.append_message(Message::user("q")).await.unwrap();
        store.append_message(assistant("a")).await.unwrap();
        store
            .append_context_edit(&question, Some("edited".into()))
            .await
            .unwrap();

        let received: Vec<(u64, String)> = records.try_iter().collect();
        let seqs: Vec<u64> = received.iter().map(|(seq, _)| *seq).collect();
        assert_eq!(seqs, [1, 2, 3]);
        let loaded = LoadedSession {
            header: store.header().clone(),
            records: received.into_iter().map(|(_, json)| json).collect(),
            repaired_bytes: 0,
        };
        let (sender, _records) = mpsc::channel();
        let reloaded = SessionStore::load(loaded, Box::new(ChannelLog(sender))).unwrap();
        assert_eq!(reloaded.entries(), store.entries());
        assert_eq!(texts(&reloaded), ["edited", "a"]);
        assert_eq!(reloaded.leaf_id(), store.leaf_id());
    }

    #[tokio::test]
    async fn records_carry_the_serialized_type_of_every_entry_kind() {
        let (sender, records) = mpsc::channel();
        let mut old = header();
        old.version = 2;
        let mut store = SessionStore::with_log(old, Box::new(ChannelLog(sender)));
        let first = store.append_message(Message::user("old")).await.unwrap();
        store.append_message(assistant("a")).await.unwrap();
        let kept = store.append_message(Message::user("recent")).await.unwrap();
        let ids: Vec<String> = store.projection().into_iter().map(|item| item.entry_id).collect();
        let plan = || CompactionPlan {
            first_kept_entry_id: kept.clone(),
            source_entry_ids: ids[..2].to_vec(),
            request: SummaryRequest {
                provider: "p".into(),
                model: "m".into(),
                instructions: "summarize".into(),
                max_tokens: 10,
            },
            input_bytes: 1,
        };
        let attempt = store.begin_compaction(plan()).await.unwrap();
        store
            .fail_compaction(&attempt, "invalid".into(), Usage::default(), None)
            .await
            .unwrap();
        let attempt = store.begin_compaction(plan()).await.unwrap();
        let summary = CompactionSummary {
            facts: vec![SummaryFact {
                text: "fact".into(),
                source_entry_ids: vec![first.clone()],
            }],
            inferences: vec![],
            plans: vec![],
        };
        store
            .append_compaction(&attempt, summary, Usage::default())
            .await
            .unwrap();
        let request = crate::ai::Request {
            model: "m".into(),
            messages: store.messages(),
            tools: vec![],
            max_tokens: None,
        };
        store
            .record_request(&request, "p", serde_json::json!({}), RequestSource::Projection)
            .await
            .unwrap();
        let mut call = AssistantMessage::failure("p", "m", StopReason::ToolUse, "");
        call.error_message = None;
        let tool_call = ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({}),
            arguments_error: None,
        };
        call.content = vec![ContentBlock::ToolCall {
            id: tool_call.id.clone(),
            name: tool_call.name.clone(),
            arguments: tool_call.arguments.clone(),
            arguments_error: None,
        }];
        store.append_message(Message::Assistant(call)).await.unwrap();
        store.append_context_edit(&first, None).await.unwrap();
        store
            .append_tool_call_started(&tool_call.id, &tool_call.name)
            .await
            .unwrap();

        let mut kinds: Vec<&str> = store.entries().iter().map(Entry::kind).collect();
        for ((_, json), entry) in records.try_iter().zip(store.entries()) {
            let value: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(value["type"], entry.kind());
            assert_eq!(value["timestamp"], entry.timestamp());
        }
        kinds.sort_unstable();
        kinds.dedup();
        assert_eq!(kinds.len(), 9, "every entry kind was checked: {kinds:?}");
    }

    #[tokio::test]
    async fn an_append_that_times_out_stops_later_writes() {
        let mut store = SessionStore::with_log(header(), Box::new(Unresponsive));
        store.append_timeout = Duration::from_millis(10);

        let error = store.append_message(Message::user("q")).await.unwrap_err();

        assert!(error.to_string().contains("did not answer within 10ms"), "{error}");
        assert!(store.entries().is_empty());
        store.log = Some(Box::new(JsonlFile::with_writer(Box::new(std::io::sink()))));
        let later = store.append_message(Message::user("later")).await.unwrap_err();
        assert!(later.to_string().contains("outcome is unknown"), "{later}");
    }

    #[tokio::test]
    async fn why_writes_stopped_can_be_told_apart() {
        assert_eq!(SessionStore::in_memory(header()).writes_stopped(), None);

        let mut failed = SessionStore::with_writer(header(), Box::new(FailingWriter));
        failed.append_message(Message::user("q")).await.unwrap_err();
        assert_eq!(
            failed.writes_stopped(),
            Some(&WritesStopped::Failed("disk full".into()))
        );

        let mut silent = SessionStore::with_log(header(), Box::new(Unresponsive));
        silent.append_timeout = Duration::from_millis(10);
        silent.append_message(Message::user("q")).await.unwrap_err();
        assert_eq!(
            silent.writes_stopped(),
            Some(&WritesStopped::TimedOut(Duration::from_millis(10)))
        );

        let mut dropped = SessionStore::with_log(header(), Box::new(Unresponsive));
        assert!(dropped.append_message(Message::user("q")).now_or_never().is_none());
        assert_eq!(dropped.writes_stopped(), Some(&WritesStopped::Interrupted));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        drop(SessionStore::create(&path, header()).unwrap());
        assert_eq!(
            SessionStore::read(&path).unwrap().writes_stopped(),
            Some(&WritesStopped::ReadOnly)
        );
    }

    #[tokio::test]
    async fn a_file_write_that_blocks_times_out_without_stalling_the_runtime() {
        /// Answers every write late, like a session volume that stopped answering; bounded,
        /// so that a store that waits for it still ends.
        struct Stuck;
        impl std::io::Write for Stuck {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                std::thread::sleep(Duration::from_secs(2));
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut store = SessionStore::with_writer(header(), Box::new(Stuck));
        store.append_timeout = Duration::from_millis(50);
        let started = std::time::Instant::now();

        let error = store.append_message(Message::user("q")).await.unwrap_err();

        assert!(error.to_string().contains("did not answer within 50ms"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
    }

    #[tokio::test]
    async fn an_append_dropped_while_waiting_stops_later_writes() {
        let mut store = SessionStore::with_log(header(), Box::new(Unresponsive));

        // Poll once: the log has the record but has not answered, so its outcome is unknown.
        assert!(store.append_message(Message::user("q")).now_or_never().is_none());

        assert!(store.entries().is_empty());
        store.log = Some(Box::new(JsonlFile::with_writer(Box::new(std::io::sink()))));
        let later = store.append_message(Message::user("later")).await.unwrap_err();
        assert!(later.to_string().contains("outcome is unknown"), "{later}");
    }

    #[tokio::test]
    async fn a_session_file_has_one_writer_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let store = SessionStore::create(&path, header()).unwrap();

        let error = SessionStore::open(&path).err().unwrap().to_string();
        assert!(error.contains("is in use"), "{error}");

        drop(store);
        let mut reopened = SessionStore::open(&path).unwrap();
        reopened.append_message(Message::user("q")).await.unwrap();
        let error = SessionStore::open(&path).err().unwrap().to_string();
        assert!(error.contains("is in use"), "{error}");
    }

    #[tokio::test]
    async fn a_snapshot_reads_a_session_in_use_without_changing_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut writer = SessionStore::create(&path, header()).unwrap();
        writer.append_message(Message::user("q")).await.unwrap();
        // A record the writer has not finished yet.
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"type\":\"mess")
            .unwrap();
        let before = std::fs::read(&path).unwrap();

        let mut snapshot = SessionStore::read(&path).unwrap();

        assert_eq!(texts(&snapshot), ["q"]);
        assert_eq!(snapshot.recovered_tail_bytes(), "{\"type\":\"mess".len());
        let error = snapshot.append_message(Message::user("x")).await.unwrap_err();
        assert!(error.to_string().contains("read-only"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(SessionStore::open(&path).is_err(), "the writer still holds the file");
    }

    #[cfg(unix)]
    #[test]
    fn a_dropped_store_can_be_reopened_while_this_process_starts_programs() {
        // A program being started holds copies of this process's descriptors until it runs,
        // so the lock of a store that was just dropped can outlive the store for a moment.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        drop(SessionStore::create(&path, header()).unwrap());
        let starting = std::sync::atomic::AtomicBool::new(true);

        let failures = std::thread::scope(|scope| {
            scope.spawn(|| {
                while starting.load(std::sync::atomic::Ordering::Relaxed) {
                    std::process::Command::new("true").status().unwrap();
                }
            });
            let failures: Vec<String> = (0..3000)
                .filter_map(|_| SessionStore::open(&path).err().map(|error| error.to_string()))
                .collect();
            starting.store(false, std::sync::atomic::Ordering::Relaxed);
            failures
        });

        assert!(
            failures.is_empty(),
            "{} reopenings failed: {:?}",
            failures.len(),
            failures.first()
        );
    }

    /// Keeps `keep` bytes of the first write and then fails once, like a disk that fills up
    /// in the middle of a record and is freed again. Later writes succeed.
    struct PartialWriteOnce {
        file: std::fs::File,
        keep: usize,
        calls: usize,
    }

    impl Write for PartialWriteOnce {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.calls += 1;
            match self.calls {
                1 => {
                    let part = &bytes[..self.keep.min(bytes.len())];
                    self.file.write_all(part)?;
                    Ok(part.len())
                }
                2 => Err(std::io::Error::other("disk full")),
                _ => self.file.write(bytes),
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.file.flush()
        }
    }

    #[tokio::test]
    async fn a_partial_write_stops_later_writes_and_leaves_a_repairable_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut store = SessionStore::create(&path, header()).unwrap();
        store.append_message(Message::user("kept")).await.unwrap();
        drop(store);
        let file = OpenOptions::new().append(true).open(&path).unwrap();
        let writer = PartialWriteOnce {
            file,
            keep: 10,
            calls: 0,
        };
        let mut store = SessionStore::with_writer(header(), Box::new(writer));

        assert!(store.append_message(Message::user("cut short")).await.is_err());
        // A run keeps emitting messages until it observes its cancellation.
        let later = store.append_message(Message::user("after the failure")).await;
        assert!(later.is_err(), "an entry was written after a failed write");
        drop(store);

        let reopened = SessionStore::open(&path).unwrap();
        assert_eq!(reopened.recovered_tail_bytes(), 10);
        assert_eq!(texts(&reopened), ["kept"]);
    }

    #[test]
    fn newer_session_versions_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.jsonl");
        std::fs::write(
            &path,
            r#"{"type":"session","version":99,"id":"h","timestamp":1,"cwd":"/","model":"m"}"#,
        )
        .unwrap();
        let error = SessionStore::open(&path).err().unwrap().to_string();
        assert!(error.contains("newer than the supported version"), "{error}");
    }

    #[test]
    fn a_newer_session_with_a_cut_short_tail_is_left_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"session\",\"version\":99,\"id\":\"h\",\"timestamp\":1,\"cwd\":\"/\",\"model\":\"m\"}\n{\"type\":\"mess",
        )
        .unwrap();
        let original = std::fs::read(&path).unwrap();

        assert!(SessionStore::open(&path).is_err());

        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn damage_before_the_last_line_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = session_file(dir.path(), &["{broken", &user_entry("a", None)]);
        assert!(SessionStore::open(&path).is_err());
    }

    #[tokio::test]
    async fn a_complete_last_line_without_newline_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = session_file(dir.path(), &[&user_entry("a", None)]);
        let mut store = SessionStore::open(&path).unwrap();
        store.append_message(Message::user("b")).await.unwrap();
        drop(store);
        assert_eq!(texts(&SessionStore::open(&path).unwrap()), ["a", "b"]);
    }

    #[tokio::test]
    async fn recovers_a_tail_cut_inside_a_utf8_character() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utf8.jsonl");
        let mut store = SessionStore::create(&path, header()).unwrap();
        store.append_message(Message::user("keep")).await.unwrap();
        store.append_message(Message::user("中文")).await.unwrap();
        drop(store);
        let bytes = std::fs::read(&path).unwrap();
        let cut = bytes.windows("中".len()).position(|w| w == "中".as_bytes()).unwrap() + 2;
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(u64::try_from(cut).unwrap())
            .unwrap();

        let reopened = SessionStore::open(&path);

        assert!(reopened.is_ok(), "a partial UTF-8 tail was not repaired");
        assert_eq!(texts(&reopened.unwrap()), ["keep"]);
    }

    #[test]
    fn does_not_delete_a_corrupt_newline_terminated_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = session_file(dir.path(), &[&user_entry("a", None), "{broken}\n"]);
        let original = std::fs::read(&path).unwrap();

        let result = SessionStore::open(&path);

        assert!(result.is_err(), "a complete corrupt record was silently discarded");
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn preserves_unknown_complete_records_even_without_a_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = session_file(dir.path(), &[&user_entry("a", None), r#"{"type":"future_kind"}"#]);
        let original = std::fs::read(&path).unwrap();

        assert!(SessionStore::open(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[tokio::test]
    async fn an_incomplete_utf8_suffix_does_not_hide_an_earlier_syntax_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invalid.jsonl");
        let mut store = SessionStore::create(&path, header()).unwrap();
        store.append_message(Message::user("keep")).await.unwrap();
        drop(store);
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{broken}\xe4\xb8")
            .unwrap();
        let original = std::fs::read(&path).unwrap();

        assert!(
            SessionStore::open(&path).is_err(),
            "invalid JSON was mistaken for an interrupted UTF-8 write"
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[tokio::test]
    async fn context_edits_cannot_target_system_messages() {
        let mut store = SessionStore::in_memory(header());
        let system = Message::System(crate::ai::SystemMessage {
            sections: vec![],
            timestamp: 0,
        });
        let id = store.append_message(system).await.unwrap();
        assert!(store.append_context_edit(&id, None).await.is_err());
    }

    #[tokio::test]
    async fn branches_select_a_path_and_edits_are_branch_local() {
        let mut store = SessionStore::in_memory(header());
        let q = store.append_message(Message::user("q")).await.unwrap();
        let a = store.append_message(assistant("a")).await.unwrap();
        store.append_context_edit(&a, None).await.unwrap();
        store.append_message(Message::user("old branch")).await.unwrap();
        assert_eq!(texts(&store), ["q", "old branch"]);

        store.branch(&a).unwrap();
        store.append_message(Message::user("new branch")).await.unwrap();
        // The edit lives on the old branch, so `a` is visible again here.
        assert_eq!(texts(&store), ["q", "a", "new branch"]);
        assert!(store.append_context_edit("missing", None).await.is_err());
        store.branch(&q).unwrap();
        assert!(
            store.append_context_edit(&a, None).await.is_err(),
            "target not on the active branch"
        );
    }

    #[cfg(unix)]
    #[test]
    fn new_session_files_are_private_to_the_owner() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private.jsonl");
        SessionStore::create(&path, header()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[tokio::test]
    async fn file_round_trip_and_recovery_from_partial_last_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        {
            let mut store = SessionStore::create(&path, header()).unwrap();
            store.append_message(Message::user("q")).await.unwrap();
            store.append_message(assistant("a")).await.unwrap();
        }
        assert!(
            SessionStore::create(&path, header()).is_err(),
            "never overwrites a session"
        );

        // Simulate a crash in the middle of writing a third entry.
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"type\":\"message\",\"id\":\"x").unwrap();
        drop(file);

        let mut store = SessionStore::open(&path).unwrap();
        assert_eq!(texts(&store), ["q", "a"]);
        store.append_message(Message::user("again")).await.unwrap();
        drop(store);
        let reopened = SessionStore::open(&path).unwrap();
        assert_eq!(texts(&reopened), ["q", "a", "again"]);
        assert_eq!(reopened.header().model, "m");
    }
}
