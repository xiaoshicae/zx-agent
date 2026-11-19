//! Append-only session tree and its projection (pi: coding-agent SessionManager).
//!
//! Every change appends an entry; stored entries are never rewritten. Entries link to their
//! parent, so the session is a tree and the "leaf" pointer selects the current branch. The
//! messages a model sees are a projection of that branch: context edits can omit or replace
//! earlier messages without touching the stored history, which stays available for auditing.
//!
//! A [`SessionStorage`] keeps the sessions, and the [`SessionLog`] it hands out keeps one
//! session's entries: memory ([`MemoryStorage`]), session files (`JsonlStorage` of the
//! zx-builtin crate), or a backend of the host's choice. Only the current format, [`SESSION_VERSION`], is read and
//! written.
//!
//! - `entry`: the header and the entries of the tree
//! - `storage` and `log`: the interfaces of where sessions are kept: a storage of sessions,
//!   and the log of one session's entries
//! - `memory`: the storage that ships with the kernel, in memory
//! - `projection`: the messages a branch projects for the model
//! - `compaction`: what a summary holds, and the records of a compaction with the rules they
//!   follow
//! - `request`: request configuration changes and per-request checkpoints
//! - `recovery`: start records of calls with side effects, and the calls an interrupted run
//!   left without results
//! - `name`: the session's name, kept in `session_info` records
//! - `test_support` and `tests`, compiled for tests only: what the session's unit tests
//!   share, and the tests of the transcript

mod compaction;
mod entry;
mod log;
mod memory;
mod name;
mod projection;
mod recovery;
mod request;
mod storage;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
pub use compaction::{
    CompactionEntry, CompactionFailedEntry, CompactionStartedEntry, CompactionSummary, SummarizedBy, SummaryFact,
    SummaryRequest,
};
pub(crate) use compaction::{CompactionPlan, MAX_SUMMARY_BYTES, summary_input, summary_messages};
pub use entry::{ContextEditEntry, CustomEntry, CustomMessage, CustomMessageEntry, Entry, MessageEntry, SessionHeader};
pub use log::{LoadedSession, LogRecord, SessionLog};
pub use memory::MemoryStorage;
pub use name::{SessionInfoEntry, SessionName};
pub use projection::ProjectedMessage;
pub(crate) use projection::{replace_text, without_details};
pub use recovery::{InterruptedToolCall, StartRecord, ToolCallStartedEntry};
pub use request::{RequestConfig, RequestConfigEntry, RequestSource, RequestStartedEntry};
pub use storage::{SessionInfo, SessionList, SessionQuery, SessionStorage, Unreadable};
#[cfg(test)]
pub(crate) use test_support::{FailingWriter, WriterLog};

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};

use crate::ai::{Message, UserMessageKind, now_ms};

/// The session format: written to every new header, and the only version a session may have
/// to be created, loaded or read. A change to the format raises it, and sessions of any
/// other version are refused.
pub const SESSION_VERSION: u32 = 13;

/// How long one append may wait for its log, such as a database that stopped answering.
/// A file append finishes long before; a timed-out append stops writing, like a failed one.
const APPEND_TIMEOUT: Duration = Duration::from_secs(30);

/// Why a transcript no longer writes, as the error of every later append says. A session
/// whose transcript stopped writing closes itself, and the agent opens it again from its
/// storage, which holds what was recorded.
#[derive(Debug, thiserror::Error)]
enum WritesStopped {
    /// An append was dropped while it waited for its log, so the log may or may not hold the
    /// record.
    #[error("an earlier append was interrupted before its log answered, so its outcome is unknown")]
    Interrupted,
    /// An append did not finish within this time; its outcome is unknown, as above.
    #[error("an earlier append did not finish within {0:?}, so its outcome is unknown")]
    TimedOut(Duration),
    /// An append failed with this error.
    #[error("an earlier write failed: {0}")]
    Failed(String),
    /// The transcript is a snapshot ([`Transcript::snapshot`]), which never writes.
    #[error("it was read as a snapshot, which is read-only")]
    ReadOnly,
}

/// Whether `message` is a prompt: what a person sent, or what a hook asked for in a
/// person's place.
fn is_prompt(message: &Message) -> bool {
    matches!(
        message,
        Message::User {
            kind: UserMessageKind::Prompt,
            ..
        }
    )
}

/// A session tree, kept in memory and optionally stored by a [`SessionLog`].
///
/// Methods that write are `async`: each waits until its log has stored the entry, and only
/// then changes memory. After a failed write, every later append fails without writing; so
/// does every append after one that timed out or whose future was dropped, because its
/// outcome is unknown. A failed write can leave a partial record; stopping there keeps it the
/// last one, which a session file repairs when the session is opened again. One transcript is the
/// only writer of its session. Appending to a log waits with a Tokio timer, so it needs a
/// Tokio runtime with time enabled.
pub struct Transcript {
    log: Option<Box<dyn SessionLog>>,
    /// Why this transcript no longer writes, if it does not.
    stopped: Option<WritesStopped>,
    append_timeout: Duration,
    header: SessionHeader,
    entries: Vec<Entry>,
    index: HashMap<String, usize>,
    /// For each entry, by position, the position of the request configuration in force there:
    /// the nearest one on its branch, itself included.
    request_configs: Vec<Option<usize>>,
    leaf: Option<String>,
    incomplete_tail_bytes: usize,
}

impl Transcript {
    /// A session kept only in this transcript: nothing stores it, so nothing can list it or open
    /// it again.
    pub fn in_memory(header: SessionHeader) -> Self {
        Self {
            log: None,
            stopped: None,
            append_timeout: APPEND_TIMEOUT,
            header,
            entries: Vec::new(),
            index: HashMap::new(),
            request_configs: Vec::new(),
            leaf: None,
            incomplete_tail_bytes: 0,
        }
    }

    /// A new session whose entries go to `log`: what [`SessionStorage::create`] returned
    /// for `header`, which the storage has stored already.
    pub fn with_log(header: SessionHeader, log: Box<dyn SessionLog>) -> Self {
        Self {
            log: Some(log),
            ..Self::in_memory(header)
        }
    }

    /// Rebuild a stored session from `loaded` and append to `log` from then on: what
    /// [`SessionStorage::open`] returned. Checks the format version, which must be
    /// [`SESSION_VERSION`], and every record. The leaf is the last entry.
    ///
    /// A crash during the final write can leave an incomplete last record, which the storage
    /// leaves out ([`Transcript::incomplete_tail_bytes`]); damage anywhere else is an error,
    /// and the stored session is left unchanged.
    pub fn load(loaded: LoadedSession, log: Box<dyn SessionLog>) -> Result<Self> {
        let mut store = Self::replay(loaded)?;
        store.log = Some(log);
        Ok(store)
    }

    /// A stored session to read, not to write: what [`SessionStorage::read`] returned, also
    /// of a session that another transcript is writing. Checked like [`Transcript::load`]; every
    /// append to the snapshot fails.
    pub fn snapshot(loaded: LoadedSession) -> Result<Self> {
        let mut store = Self::replay(loaded)?;
        store.stopped = Some(WritesStopped::ReadOnly);
        Ok(store)
    }

    fn replay(loaded: LoadedSession) -> Result<Self> {
        let LoadedSession {
            header,
            records,
            incomplete_tail_bytes,
        } = loaded;
        header.check_version()?;
        let mut store = Self::in_memory(header);
        for (position, record) in (1..).zip(&records) {
            let entry: Entry = serde_json::from_str(record).map_err(|error| anyhow!("entry {position}: {error}"))?;
            store
                .validate_entry(&entry)
                .map_err(|error| anyhow!("entry {position}: {error:#}"))?;
            store.insert(entry);
        }
        store.incomplete_tail_bytes = incomplete_tail_bytes;
        Ok(store)
    }

    /// Whether an append failed, timed out or was interrupted, so that this transcript, which
    /// wrote until then, no longer agrees with its log. A snapshot never wrote and does not
    /// count.
    pub(crate) fn lost_its_log(&self) -> bool {
        match self.stopped {
            Some(WritesStopped::Interrupted | WritesStopped::TimedOut(_) | WritesStopped::Failed(_)) => true,
            Some(WritesStopped::ReadOnly) | None => false,
        }
    }

    /// A session that writes entries to `writer` instead of a storage, so tests can make
    /// writes fail. The header is not written.
    #[cfg(test)]
    pub(crate) fn with_writer(header: SessionHeader, writer: Box<dyn std::io::Write + Send + Sync>) -> Self {
        Self::with_log(header, Box::new(WriterLog::new(writer)))
    }

    /// The header the session was created with.
    pub fn header(&self) -> &SessionHeader {
        &self.header
    }

    /// Bytes of an incomplete final record that the storage left out when the session was
    /// loaded; zero if there was none. A session file cuts them off before its next record.
    /// Complete corrupt records are errors and are never silently removed.
    pub fn incomplete_tail_bytes(&self) -> usize {
        self.incomplete_tail_bytes
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
        self.append_message_from(message, None).await
    }

    /// Append `message` as [`Self::append_message`] does, with `actor`, the person who sent
    /// it as the host names its people. Only a person's input has one: an actor with any
    /// other message is an error, and nothing is written.
    pub async fn append_message_from(&mut self, message: Message, actor: Option<String>) -> Result<String> {
        let entry = Entry::Message(MessageEntry {
            id: self.new_id(),
            parent_id: self.leaf.clone(),
            timestamp: now_ms(),
            message,
            actor,
        });
        self.append(entry).await
    }

    /// Who sent the latest input of the current branch: the actor recorded with its last
    /// prompt, the message a person sent or a hook asked for. `None` when that message names
    /// nobody, and when the branch has no prompt.
    pub fn latest_actor(&self) -> Option<&str> {
        self.ancestors(self.leaf.as_deref()).find_map(|entry| match entry {
            Entry::Message(entry) if is_prompt(&entry.message) => Some(entry.actor.as_deref()),
            _ => None,
        })?
    }

    /// Omit (`None`) or replace the text of a message on the current branch, for future
    /// model context only. The target keeps its original content in the log.
    /// System and synthetic summary messages cannot be edited directly. Editing a record that
    /// a summary read corrects its evidence: the summary is then invalid as a whole and the
    /// originals are projected again.
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
    /// `custom_type` must not be blank.
    pub async fn append_custom_entry(&mut self, custom_type: &str, data: serde_json::Value) -> Result<String> {
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
    /// on (pi: `sendMessage`). The message's `custom_type` must not be blank.
    pub async fn append_custom_message(&mut self, message: CustomMessage) -> Result<String> {
        let entry = Entry::CustomMessage(CustomMessageEntry {
            id: self.new_id(),
            parent_id: self.leaf.clone(),
            timestamp: now_ms(),
            message,
        });
        self.append(entry).await
    }

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
            Entry::Message(message) => ensure!(
                message.actor.is_none() || is_prompt(&message.message),
                "entry {}: only a person's input names an actor",
                message.id
            ),
            // A name is valid by its type, whether appended or loaded.
            Entry::SessionInfo(_) => {}
            Entry::Custom(custom) => ensure_custom_type(&custom.custom_type)?,
            Entry::CustomMessage(custom) => ensure_custom_type(&custom.message.custom_type)?,
            Entry::ContextEdit(edit) => self.validate_context_edit(edit)?,
            Entry::CompactionStarted(started) => self.validate_compaction_start(started)?,
            Entry::Compaction(done) => self.validate_compaction(done)?,
            Entry::CompactionFailed(failed) => self.validate_compaction_failure(failed)?,
            Entry::RequestConfig(config) => Self::validate_request_config(config)?,
            Entry::RequestStarted(started) => self.validate_request_start(started)?,
            Entry::ToolCallStarted(start) => self.validate_tool_call_start(start)?,
        }
        Ok(())
    }

    /// The rules an edit follows, when it is appended and when a session is loaded: it
    /// targets an earlier message on its own branch, one that an edit may change
    /// ([`context_edit_refusal`]).
    fn validate_context_edit(&self, edit: &ContextEditEntry) -> Result<()> {
        let target = edit.target_id.as_str();
        match self.get(target) {
            Some(Entry::Message(entry)) => {
                if let Some(reason) = context_edit_refusal(&entry.message, edit.replacement.is_some()) {
                    bail!("entry {target} {reason}");
                }
            }
            // A custom message is a user message to the model, edited like one.
            Some(Entry::CustomMessage(_)) => {}
            Some(
                Entry::ContextEdit(_)
                | Entry::CompactionStarted(_)
                | Entry::Compaction(_)
                | Entry::CompactionFailed(_)
                | Entry::RequestConfig(_)
                | Entry::RequestStarted(_)
                | Entry::ToolCallStarted(_)
                | Entry::Custom(_)
                | Entry::SessionInfo(_),
            ) => bail!("entry {target} is not a message"),
            None => bail!("entry {target} not found"),
        }
        ensure!(
            self.ancestors(edit.parent_id.as_deref())
                .any(|entry| entry.id() == target),
            "entry {target} is not on the edit's branch"
        );
        Ok(())
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
    /// the next append is a new root. As with [`Transcript::branch`], nothing is written.
    pub fn reset_leaf(&mut self) {
        self.leaf = None;
    }

    /// The header of a session that continues one of this session's branches, for
    /// [`Transcript::copy_branch`]: a fresh ID and start time, the given working directory
    /// and model, this session's labels, and this session as `parent_session`.
    pub fn fork_header(&self, cwd: &Path, model: &str) -> SessionHeader {
        SessionHeader {
            parent_session: Some(self.header.id.clone()),
            labels: self.header.labels.clone(),
            ..SessionHeader::new(cwd, model)
        }
    }

    /// Append the entries of `source` from its root to `leaf`, unchanged: the same IDs,
    /// parents, timestamps and content, so the projection, compactions and request audit of
    /// that branch carry over. Returns how many entries were copied. Fails without writing
    /// when this session already has entries or `leaf` is not in `source`. A failed append
    /// stops writing, as any append does.
    ///
    /// A branch that ends with calls without results, as one that ends at the answer that
    /// issued them does, gets a start record of its own for each call that `source` recorded
    /// as started after `leaf`: the copy says of those calls what `source` says once it goes
    /// back to `leaf`, that they began.
    pub async fn copy_branch(&mut self, source: &Self, leaf: &str) -> Result<usize> {
        if !self.entries.is_empty() {
            bail!("a branch can only be copied into an empty session");
        }
        if source.get(leaf).is_none() {
            bail!("entry {leaf} not found");
        }
        let path = source.branch_path_from(Some(leaf));
        for entry in &path {
            self.append((*entry).clone()).await?;
        }
        // The same calls in the same order, of the same entries: only what each session
        // knows of their starts can differ.
        let known = source.interrupted_tool_calls_at(Some(leaf));
        for (known, copied) in known.into_iter().zip(self.interrupted_tool_calls()) {
            if known.start == StartRecord::Present && copied.start == StartRecord::Absent {
                self.append_tool_call_started(&known.tool_call_id, &known.tool_name)
                    .await?;
            }
        }
        Ok(path.len())
    }

    /// Entries from the root to the leaf.
    pub fn branch_path(&self) -> Vec<&Entry> {
        self.branch_path_from(self.leaf.as_deref())
    }

    /// The branch that ends at `leaf`, from the root to `leaf`.
    fn branch_path_from(&self, leaf: Option<&str>) -> Vec<&Entry> {
        let mut path: Vec<&Entry> = self.ancestors(leaf).collect();
        path.reverse();
        path
    }

    /// The branch that ends at `leaf`, walked upward: `leaf` first, the root last.
    fn ancestors(&self, leaf: Option<&str>) -> impl Iterator<Item = &Entry> {
        std::iter::successors(leaf.and_then(|id| self.get(id)), |entry| {
            entry.parent_id().and_then(|id| self.get(id))
        })
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
            // Writing does not limit how deep a value is nested and reading does: a record
            // that could not be read back would leave the whole session unreadable.
            serde_json::from_str::<Entry>(&json).context("the entry could not be read back")?;
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
        let request_config = match &entry {
            Entry::RequestConfig(_) => Some(self.entries.len()),
            _ => entry
                .parent_id()
                .and_then(|id| self.index.get(id))
                .and_then(|&parent| self.request_configs[parent]),
        };
        self.request_configs.push(request_config);
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

/// Why a context edit may not change `message`, if it may not. A system message is never
/// edited: prompt changes are new system messages. A text replacement (`replaces_text`) does
/// not erase tool calls, whose results would otherwise lose their call.
///
/// The transcript applies this to every edit it records or loads, and the hook chain to a whole
/// plan of edits before any of them is recorded, so both refuse the same edits.
pub(crate) fn context_edit_refusal(message: &Message, replaces_text: bool) -> Option<&'static str> {
    match message {
        Message::System(_) => Some("is a system message; record prompt changes as new system messages"),
        Message::Assistant(answer) if replaces_text && !answer.tool_calls().is_empty() => {
            Some("has tool calls, which a text replacement would erase")
        }
        Message::Assistant(_) | Message::User { .. } | Message::ToolResult(_) => None,
    }
}

/// A custom type names what an extension recorded; a blank one would name nothing.
fn ensure_custom_type(custom_type: &str) -> Result<()> {
    if custom_type.trim().is_empty() {
        bail!("a custom record needs a customType");
    }
    Ok(())
}
