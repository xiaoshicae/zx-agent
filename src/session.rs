//! Append-only JSONL session tree and its projection (pi: coding-agent SessionManager).
//!
//! Every change appends an entry; written entries are never rewritten. The one exception is
//! `open`, which cuts off an unfinished last line left by a crash or failed write, and
//! reports how many bytes it removed. Entries link to their parent, so
//! the file is a tree and the "leaf" pointer selects the current branch. The messages a
//! model sees are a projection of that branch: context edits can omit or replace earlier
//! messages without touching the raw history, which stays available for auditing.
//!
//! Not implemented yet: compaction entries (pi: CompactionEntry).

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

#[cfg(test)]
use crate::ai::ContentBlock;
use crate::ai::{Message, now_ms};

/// Format version written to new session headers. Version 2 records the system prompt as
/// system messages in the conversation; version 1 kept it in the header. Version 1 files still
/// open: the old header field is ignored, and the next run records the prompt as a message.
pub const SESSION_VERSION: u32 = 2;

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

/// One line of the session file after the header.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Entry {
    Message(MessageEntry),
    ContextEdit(ContextEditEntry),
}

impl Entry {
    /// The entry's id.
    pub fn id(&self) -> &str {
        match self {
            Entry::Message(entry) => &entry.id,
            Entry::ContextEdit(entry) => &entry.id,
        }
    }

    /// The parent entry's id; `None` for a root entry.
    pub fn parent_id(&self) -> Option<&str> {
        match self {
            Entry::Message(entry) => entry.parent_id.as_deref(),
            Entry::ContextEdit(entry) => entry.parent_id.as_deref(),
        }
    }
}

/// A model-visible message and the entry it came from.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectedMessage {
    pub entry_id: String,
    pub message: Message,
}

/// A session tree, kept in memory and optionally mirrored to a JSONL file.
///
/// After a failed write, every later append fails without writing. A failed write can
/// leave a partial record; stopping there keeps it the last line, which `open` repairs.
pub struct SessionStore {
    path: Option<PathBuf>,
    /// Receives one JSON line per entry; the session file outside of tests.
    writer: Option<Box<dyn Write + Send>>,
    /// The first write error; set once, after which nothing more is written.
    write_failure: Option<String>,
    header: SessionHeader,
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
            writer: None,
            write_failure: None,
            header,
            entries: Vec::new(),
            index: HashMap::new(),
            leaf: None,
            recovered_tail_bytes: 0,
        }
    }

    /// Create a new session file. Fails if the file exists. Unlike pi, the header is written
    /// immediately, so even a run that crashes before the first answer leaves a record.
    pub fn create(path: &Path, header: SessionHeader) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        // Tool output in the session can contain secrets, so only the owner may read the file.
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options
            .open(path)
            .with_context(|| format!("cannot create session file {}", path.display()))?;
        writeln!(file, "{}", serde_json::to_string(&HeaderLine::Session(header.clone()))?)?;
        file.flush()?;
        Ok(Self {
            path: Some(path.to_path_buf()),
            writer: Some(Box::new(file)),
            ..Self::in_memory(header)
        })
    }

    /// Open an existing session file for reading and appending. The leaf is the last entry.
    ///
    /// A crash during the final write can leave an incomplete last line. That line is cut
    /// off so later appends start on a clean line; damage anywhere else is an error.
    pub fn open(path: &Path) -> Result<Self> {
        // A short final write can end inside a UTF-8 character, so validate one record at
        // a time rather than rejecting the whole file during UTF-8 decoding.
        let content = std::fs::read(path).with_context(|| format!("cannot read session file {}", path.display()))?;
        let mut lines = Vec::new();
        let mut offset = 0;
        for raw in content.split_inclusive(|&byte| byte == b'\n') {
            lines.push((offset, raw));
            offset += raw.len();
        }
        let lines: Vec<(usize, &[u8])> = lines
            .into_iter()
            .filter(|(_, raw)| !raw.iter().all(u8::is_ascii_whitespace))
            .collect();
        let (_, first) = lines
            .first()
            .ok_or_else(|| anyhow!("session file {} is empty", path.display()))?;
        let HeaderLine::Session(header) =
            serde_json::from_slice(first).with_context(|| format!("{}: invalid session header", path.display()))?;
        if header.version > SESSION_VERSION {
            bail!(
                "{}: session version {} is newer than the supported version {SESSION_VERSION}",
                path.display(),
                header.version
            );
        }

        let mut store = Self::in_memory(header);
        let mut truncate_at = None;
        for (position, (line_offset, raw)) in lines.iter().enumerate().skip(1) {
            match serde_json::from_slice::<Entry>(raw) {
                Ok(entry) => store.insert(entry),
                Err(e) if position + 1 == lines.len() && incomplete_tail(raw, &e) => {
                    truncate_at = Some(*line_offset);
                }
                Err(e) => bail!("{}: entry {position}: {e}", path.display()),
            }
        }

        let file = OpenOptions::new()
            .append(true)
            .open(path)
            .with_context(|| format!("cannot open {} for appending", path.display()))?;
        if let Some(length) = truncate_at {
            file.set_len(u64::try_from(length)?)?;
            store.recovered_tail_bytes = content.len() - length;
        } else if !content.is_empty() && !content.ends_with(b"\n") {
            // A complete last entry without its newline: terminate it before appending.
            (&file).write_all(b"\n")?;
        }
        store.path = Some(path.to_path_buf());
        store.writer = Some(Box::new(file));
        Ok(store)
    }

    /// A session that writes entries to `writer` instead of a file, so tests can make writes
    /// fail. The header is not written.
    #[cfg(test)]
    pub(crate) fn with_writer(header: SessionHeader, writer: Box<dyn Write + Send>) -> Self {
        Self {
            writer: Some(writer),
            ..Self::in_memory(header)
        }
    }

    /// The header the session was created with.
    pub fn header(&self) -> &SessionHeader {
        &self.header
    }

    /// The session file; `None` for an in-memory session.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Bytes removed from an incomplete final record when opened; zero if no repair was
    /// needed. Complete corrupt records are errors and are never silently removed.
    pub fn recovered_tail_bytes(&self) -> usize {
        self.recovered_tail_bytes
    }

    /// Every entry in file order, on all branches.
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
    pub fn append_message(&mut self, message: Message) -> Result<String> {
        let entry = Entry::Message(MessageEntry {
            id: self.new_id(),
            parent_id: self.leaf.clone(),
            timestamp: now_ms(),
            message,
        });
        self.append(entry)
    }

    /// Omit (`None`) or replace the text of a message on the current branch, for future
    /// model context only. The target keeps its original content in the file.
    pub fn append_context_edit(&mut self, target_id: &str, replacement: Option<String>) -> Result<String> {
        match self.get(target_id) {
            Some(Entry::Message(entry)) if matches!(entry.message, Message::System(_)) => {
                bail!("entry {target_id} is a system message; record prompt changes as new system messages")
            }
            Some(Entry::Message(_)) => {}
            Some(Entry::ContextEdit(_)) => bail!("entry {target_id} is a context edit, not a message"),
            None => bail!("entry {target_id} not found"),
        }
        if !self.branch_path().iter().any(|entry| entry.id() == target_id) {
            bail!("entry {target_id} is not on the active branch");
        }
        let entry = Entry::ContextEdit(ContextEditEntry {
            id: self.new_id(),
            parent_id: self.leaf.clone(),
            timestamp: now_ms(),
            target_id: target_id.to_string(),
            replacement,
        });
        self.append(entry)
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
        let mut path = Vec::new();
        let mut current = self.leaf.as_deref().and_then(|id| self.get(id));
        while let Some(entry) = current {
            path.push(entry);
            current = entry.parent_id().and_then(|id| self.get(id));
        }
        path.reverse();
        path
    }

    /// The model-visible messages of the current branch, with their source entries.
    pub fn projection(&self) -> Vec<ProjectedMessage> {
        let path = self.branch_path();
        // Later edits of the same target win.
        let mut edits: HashMap<&str, &Option<String>> = HashMap::new();
        for entry in &path {
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

    fn append(&mut self, entry: Entry) -> Result<String> {
        // Synchronous I/O, although callers run inside async tasks: one short line per message
        // finishes in microseconds, and writing it before continuing is what makes the audit
        // trail reliable.
        if let Some(failure) = &self.write_failure {
            // Appending after a partial line would glue the next entry to it, turning a
            // repairable tail into a corrupt record in the middle of the file.
            bail!("session file is not written after an earlier write failure: {failure}");
        }
        if let Some(writer) = &mut self.writer {
            // Write before updating memory: an entry missing from the file must never become
            // the parent of later entries (pi: SessionManager._appendEntry).
            let mut line = serde_json::to_string(&entry)?;
            line.push('\n');
            let written = writer.write_all(line.as_bytes()).and_then(|()| writer.flush());
            if let Err(error) = written {
                self.write_failure = Some(error.to_string());
                return Err(error).context("cannot write session entry");
            }
        }
        let id = entry.id().to_string();
        self.insert(entry);
        Ok(id)
    }

    fn insert(&mut self, entry: Entry) {
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

fn incomplete_tail(raw: &[u8], error: &serde_json::Error) -> bool {
    if raw.ends_with(b"\n") {
        return false;
    }
    if error.is_eof() {
        return true;
    }
    // Only a valid, unfinished JSON prefix may be repaired. An incomplete UTF-8
    // suffix must not hide an unrelated syntax/schema error earlier in the record.
    match std::str::from_utf8(raw) {
        Err(utf8) if utf8.error_len().is_none() => {
            serde_json::from_slice::<Entry>(&raw[..utf8.valid_up_to()]).is_err_and(|error| error.is_eof())
        }
        Ok(_) | Err(_) => false,
    }
}

/// A writer whose every write fails, like a full disk.
#[cfg(test)]
pub(crate) struct FailingWriter;

#[cfg(test)]
impl Write for FailingWriter {
    fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("disk full"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AssistantMessage, StopReason};

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

    #[test]
    fn context_edits_omit_or_replace_and_latest_wins() {
        let mut store = SessionStore::in_memory(header());
        store.append_message(Message::user("q")).unwrap();
        let failed = store.append_message(assistant("bad")).unwrap();
        let answer = store.append_message(assistant("good")).unwrap();
        store.append_context_edit(&failed, None).unwrap();
        assert_eq!(texts(&store), ["q", "good"]);
        store.append_context_edit(&answer, Some("first".into())).unwrap();
        store.append_context_edit(&answer, Some("second".into())).unwrap();
        assert_eq!(texts(&store), ["q", "second"]);
        assert_eq!(store.entries().len(), 6, "raw history keeps every entry");
    }

    #[test]
    fn a_failed_write_leaves_the_tree_unchanged() {
        let mut store = SessionStore::with_writer(header(), Box::new(FailingWriter));
        assert!(store.append_message(Message::user("q")).is_err());
        assert!(store.entries().is_empty());
        assert_eq!(store.leaf_id(), None);
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

    #[test]
    fn a_partial_write_stops_later_writes_and_leaves_a_repairable_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut store = SessionStore::create(&path, header()).unwrap();
        store.append_message(Message::user("kept")).unwrap();
        drop(store);
        let file = OpenOptions::new().append(true).open(&path).unwrap();
        let writer = PartialWriteOnce {
            file,
            keep: 10,
            calls: 0,
        };
        let mut store = SessionStore::with_writer(header(), Box::new(writer));

        assert!(store.append_message(Message::user("cut short")).is_err());
        // A run keeps emitting messages until it observes its cancellation.
        let later = store.append_message(Message::user("after the failure"));
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
    fn damage_before_the_last_line_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = session_file(dir.path(), &["{broken", &user_entry("a", None)]);
        assert!(SessionStore::open(&path).is_err());
    }

    #[test]
    fn a_complete_last_line_without_newline_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = session_file(dir.path(), &[&user_entry("a", None)]);
        let mut store = SessionStore::open(&path).unwrap();
        store.append_message(Message::user("b")).unwrap();
        assert_eq!(texts(&SessionStore::open(&path).unwrap()), ["a", "b"]);
    }

    #[test]
    fn recovers_a_tail_cut_inside_a_utf8_character() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utf8.jsonl");
        let mut store = SessionStore::create(&path, header()).unwrap();
        store.append_message(Message::user("keep")).unwrap();
        store.append_message(Message::user("中文")).unwrap();
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

    #[test]
    fn an_incomplete_utf8_suffix_does_not_hide_an_earlier_syntax_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invalid.jsonl");
        let mut store = SessionStore::create(&path, header()).unwrap();
        store.append_message(Message::user("keep")).unwrap();
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

    #[test]
    fn context_edits_cannot_target_system_messages() {
        let mut store = SessionStore::in_memory(header());
        let system = Message::System(crate::ai::SystemMessage {
            sections: vec![],
            timestamp: 0,
        });
        let id = store.append_message(system).unwrap();
        assert!(store.append_context_edit(&id, None).is_err());
    }

    #[test]
    fn branches_select_a_path_and_edits_are_branch_local() {
        let mut store = SessionStore::in_memory(header());
        let q = store.append_message(Message::user("q")).unwrap();
        let a = store.append_message(assistant("a")).unwrap();
        store.append_context_edit(&a, None).unwrap();
        store.append_message(Message::user("old branch")).unwrap();
        assert_eq!(texts(&store), ["q", "old branch"]);

        store.branch(&a).unwrap();
        store.append_message(Message::user("new branch")).unwrap();
        // The edit lives on the old branch, so `a` is visible again here.
        assert_eq!(texts(&store), ["q", "a", "new branch"]);
        assert!(store.append_context_edit("missing", None).is_err());
        store.branch(&q).unwrap();
        assert!(
            store.append_context_edit(&a, None).is_err(),
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

    #[test]
    fn file_round_trip_and_recovery_from_partial_last_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        {
            let mut store = SessionStore::create(&path, header()).unwrap();
            store.append_message(Message::user("q")).unwrap();
            store.append_message(assistant("a")).unwrap();
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
        store.append_message(Message::user("again")).unwrap();
        let reopened = SessionStore::open(&path).unwrap();
        assert_eq!(texts(&reopened), ["q", "a", "again"]);
        assert_eq!(reopened.header().model, "m");
    }
}
