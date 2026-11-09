//! Append-only session tree and its projection (pi: coding-agent SessionManager).
//!
//! Every change appends an entry; stored entries are never rewritten. Entries link to their
//! parent, so the session is a tree and the "leaf" pointer selects the current branch. The
//! messages a model sees are a projection of that branch: context edits can omit or replace
//! earlier messages without touching the raw history, which stays available for auditing.
//!
//! A [`SessionLog`] keeps the entries: the JSONL session file by default, or a backend of
//! the host's choice. The format's history is with [`SESSION_VERSION`].
//!
//! - `entry`: the header and the entries of the tree
//! - `log` and `jsonl`: where the entries are kept, and the JSONL session file
//! - `projection`: the messages a branch projects for the model
//! - `summary`: the records of summaries and the rules they follow
//! - `request`: request configuration changes and per-request checkpoints
//! - `recovery`: start records of calls with side effects, and the calls an interrupted run
//!   left without results
//! - `directory`: session files in one directory: listing them and finding one to resume
//! - `test_support` and `tests`, compiled for tests only: what the session's unit tests
//!   share, and the tests of the store

mod directory;
mod entry;
mod jsonl;
mod log;
mod projection;
mod recovery;
mod request;
mod summary;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
pub use directory::{SessionInfo, SessionList, Unreadable, list_sessions};
pub use entry::{
    ContextEditEntry, CustomEntry, CustomMessage, CustomMessageEntry, Entry, FormatUpgradeEntry, MessageEntry,
    ProjectedMessage, SessionHeader,
};
pub use log::{LoadedSession, LogRecord, SessionLog};
pub(crate) use projection::without_details;
pub use recovery::{InterruptedToolCall, StartRecord, ToolCallStartedEntry};
pub use request::{RequestConfig, RequestConfigEntry, RequestSource, RequestStartedEntry};
pub use summary::{
    CompactionEntry, CompactionFailedEntry, CompactionPlan, CompactionStartedEntry, CompactionSummary, MergedSummary,
    SummaryFact, SummaryMode, SummaryRequest,
};
pub(crate) use summary::{MAX_STATEMENTS, MAX_SUMMARY_BYTES, cite_only, summary_messages, superseded_by};
#[cfg(test)]
pub(crate) use test_support::FailingWriter;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, anyhow, bail, ensure};

use crate::ai::{Message, now_ms};
use entry::HeaderLine;
use jsonl::JsonlFile;

/// Format version written to new session headers. Version 6 adds the `custom` and
/// `custom_message` records of extensions. Version 5 adds incremental summaries. Version 4
/// adds `tool_call_started`. Version 3 adds auditable compaction, request records and
/// append-only format upgrades. Version 2 records the system prompt as system messages in
/// the conversation; version 1 kept it in the header. Version 1 files still open: the old
/// header field is ignored, and the next run records the prompt as a message.
pub const SESSION_VERSION: u32 = 6;

/// The format that adds auditable compaction, request records and append-only upgrades.
const COMPACTION_VERSION: u32 = 3;
/// The format that records that a tool call with side effects started.
const START_RECORDS_VERSION: u32 = 4;
/// The format that adds incremental summaries: `mode` and `continues` on attempts, fact IDs
/// and `supersedes`, and `superseded` and `evicted` on results.
const INCREMENTAL_SUMMARY_VERSION: u32 = 5;
/// First format version with the `custom` and `custom_message` records of extensions.
const CUSTOM_RECORDS_VERSION: u32 = 6;

/// How long one append may wait for its log, such as a database that stopped answering.
/// A file append finishes long before; a timed-out append stops writing, like a failed one.
const APPEND_TIMEOUT: Duration = Duration::from_secs(30);

/// Why a store no longer writes. Each case asks for something different before a session can
/// be written again, so hosts such as a server that keeps sessions open can match on it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WritesStopped {
    /// An append was dropped while it waited for its log, so the log may or may not hold the
    /// record. Open the session again from its storage before writing.
    #[error("an earlier append was interrupted before its log answered, so its outcome is unknown")]
    Interrupted,
    /// An append did not finish within this time; its outcome is unknown, as above.
    #[error("an earlier append did not finish within {0:?}, so its outcome is unknown")]
    TimedOut(Duration),
    /// An append failed with this error. Remove the cause, then open the session again.
    #[error("an earlier write failed: {0}")]
    Failed(String),
    /// The store is a snapshot from [`SessionStore::read`], which never writes.
    #[error("it was read as a snapshot, which is read-only")]
    ReadOnly,
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

    /// Create a new session file, with its header written at once. Fails if the file exists.
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
        let loaded = jsonl::read(path)?;
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
    /// The projected messages whose entries were appended at position `from` or later, so
    /// that a run reports only its own answer and not an earlier run's.
    pub(crate) fn messages_since(&self, from: usize) -> Vec<Message> {
        let added: HashSet<&str> = self.entries[from..]
            .iter()
            .filter_map(|entry| entry.projected().map(|(id, _)| id))
            .collect();
        self.projection()
            .into_iter()
            .filter(|projected| added.contains(projected.entry_id.as_str()))
            .map(|projected| projected.message)
            .collect()
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
    /// System and synthetic summary messages cannot be edited directly. Editing a record that
    /// a summary read corrects its evidence: a full summary is then invalid as a whole and
    /// the originals are projected again, while an incremental summary loses only the facts
    /// that cite the record.
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

    /// Record state of an extension, which never reaches the model (pi: `appendEntry`).
    /// Older sessions get a format upgrade to v6 first. `custom_type` must not be blank.
    pub async fn append_custom_entry(&mut self, custom_type: &str, data: serde_json::Value) -> Result<String> {
        ensure_custom_type(custom_type)?;
        self.ensure_format(CUSTOM_RECORDS_VERSION).await?;
        let entry = Entry::Custom(CustomEntry {
            id: self.new_id(),
            parent_id: self.leaf.clone(),
            timestamp: now_ms(),
            custom_type: custom_type.to_string(),
            data,
        });
        self.append(entry).await
    }

    /// Record a message from an extension, which the model sees as a user message from now
    /// on (pi: `sendMessage`). Older sessions get a format upgrade to v6 first. The
    /// message's `custom_type` must not be blank.
    pub async fn append_custom_message(&mut self, message: CustomMessage) -> Result<String> {
        ensure_custom_type(&message.custom_type)?;
        self.ensure_format(CUSTOM_RECORDS_VERSION).await?;
        let entry = Entry::CustomMessage(CustomMessageEntry {
            id: self.new_id(),
            parent_id: self.leaf.clone(),
            timestamp: now_ms(),
            message,
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
            // A custom message is a user message to the model, edited like one.
            Some(Entry::CustomMessage(_)) => {}
            Some(
                Entry::ContextEdit(_)
                | Entry::FormatUpgrade(_)
                | Entry::CompactionStarted(_)
                | Entry::Compaction(_)
                | Entry::CompactionFailed(_)
                | Entry::RequestConfig(_)
                | Entry::RequestStarted(_)
                | Entry::ToolCallStarted(_)
                | Entry::Custom(_),
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

    /// Move the leaf before the first entry (pi: `resetLeaf`): the projection is empty and
    /// the next append is a new root. As with [`SessionStore::branch`], nothing is written.
    pub fn reset_leaf(&mut self) {
        self.leaf = None;
    }

    /// The header of a session that continues one of this session's branches, for
    /// [`SessionStore::copy_branch`]: a fresh ID and start time, the given working directory
    /// and model, this session's file as `parent_session`, and this session's format
    /// version, which the copied entries were written under.
    pub fn fork_header(&self, cwd: &Path, model: &str) -> SessionHeader {
        SessionHeader {
            version: self.header.version,
            parent_session: self.path.as_ref().map(|path| path.display().to_string()),
            ..SessionHeader::new(cwd, model)
        }
    }

    /// Append the entries of `source` from its root to `leaf`, unchanged: the same IDs,
    /// parents, timestamps and content, so the projection, compactions and request audit of
    /// that branch carry over. Returns how many entries were
    /// copied. Fails without writing when this session already has entries, `leaf` is not
    /// in `source`, or the format versions of the two headers differ (see
    /// [`SessionStore::fork_header`]). A failed append stops writing, as any append does.
    pub async fn copy_branch(&mut self, source: &Self, leaf: &str) -> Result<usize> {
        if !self.entries.is_empty() {
            bail!("a branch can only be copied into an empty session");
        }
        if source.get(leaf).is_none() {
            bail!("entry {leaf} not found");
        }
        if self.header.version != source.header.version {
            bail!(
                "the session has format version {} but its source {}; start it with fork_header",
                self.header.version,
                source.header.version
            );
        }
        let path = source.branch_path_from(Some(leaf));
        for entry in &path {
            self.append((*entry).clone()).await?;
        }
        Ok(path.len())
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
            // the parent of later entries.
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

/// A custom type names what an extension recorded; a blank one would name nothing.
fn ensure_custom_type(custom_type: &str) -> Result<()> {
    if custom_type.trim().is_empty() {
        bail!("a custom record needs a customType");
    }
    Ok(())
}

impl SessionStore {
    /// The rules every record follows, when it is appended and when a session is loaded.
    fn validate_entry(&self, entry: &Entry) -> Result<()> {
        ensure!(
            !entry.id().is_empty() && !self.index.contains_key(entry.id()),
            "duplicate or empty entry ID {:?}",
            entry.id()
        );
        if let Some(parent) = entry.parent_id() {
            ensure!(self.index.contains_key(parent), "entry parent {parent:?} not found");
        }
        match entry {
            Entry::Message(_) => {}
            Entry::Custom(custom) => {
                ensure!(
                    self.format_version >= CUSTOM_RECORDS_VERSION,
                    "custom records require a format upgrade to v{CUSTOM_RECORDS_VERSION}"
                );
                ensure_custom_type(&custom.custom_type)?;
            }
            Entry::CustomMessage(custom) => {
                ensure!(
                    self.format_version >= CUSTOM_RECORDS_VERSION,
                    "custom records require a format upgrade to v{CUSTOM_RECORDS_VERSION}"
                );
                ensure_custom_type(&custom.message.custom_type)?;
            }
            Entry::ContextEdit(edit) => self.validate_context_edit(edit)?,
            Entry::FormatUpgrade(upgrade) => ensure!(
                upgrade.version >= self.format_version && upgrade.version <= SESSION_VERSION,
                "unsupported format upgrade {}",
                upgrade.version
            ),
            Entry::CompactionStarted(started) => self.validate_compaction_started(started)?,
            Entry::Compaction(done) => self.validate_compaction(done, entry.parent_id())?,
            Entry::CompactionFailed(failed) => {
                self.validate_attempt_completion(&failed.attempt_id, entry.parent_id())?;
            }
            Entry::RequestConfig(_) | Entry::RequestStarted(_) => self.validate_request_entry(entry)?,
            Entry::ToolCallStarted(start) => {
                ensure!(
                    self.format_version >= START_RECORDS_VERSION,
                    "tool call start records require a format upgrade to v{START_RECORDS_VERSION}"
                );
                self.validate_tool_call_start(start)?;
            }
        }
        Ok(())
    }

    /// Effective file format, including appended upgrades.
    pub fn format_version(&self) -> u32 {
        self.format_version
    }

    /// Append a format upgrade before the first record that needs `version`.
    /// The header is never rewritten.
    async fn ensure_format(&mut self, version: u32) -> Result<()> {
        if self.format_version < version {
            self.append(Entry::FormatUpgrade(FormatUpgradeEntry {
                id: self.new_id(),
                parent_id: self.leaf.clone(),
                timestamp: now_ms(),
                version,
            }))
            .await?;
        }
        Ok(())
    }
}
