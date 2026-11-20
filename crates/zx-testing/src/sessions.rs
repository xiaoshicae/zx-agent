//! Where a test's session comes from and where it is kept. An agent creates or opens every
//! test session, as it does a host's ([`Start`]); a session with history is a [`Seed`],
//! which the test writes first through the storage that keeps it, in memory or in a session
//! file, records that only the kernel writes during a run included. A [`LogStorage`] gives a
//! session the log a test chooses, such as a [`RefusingLog`].

use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use zx_agent::SessionOptions;
use zx_agent::ai::{Message, now_ms};
use zx_agent::builtin::jsonl::JsonlStorage;
use zx_agent::transcript::{
    Entry, LoadedSession, LogRecord, MemoryStorage, SessionHeader, SessionList, SessionLog, SessionQuery,
    SessionStorage, ToolCallStartedEntry, Transcript,
};

/// The header of a new session that works in the current directory.
pub fn header() -> SessionHeader {
    header_in(Path::new("."))
}

/// The header of a new session that works in `cwd`.
pub fn header_in(cwd: &Path) -> SessionHeader {
    SessionHeader::new(cwd, "test-model")
}

/// Which session the runs of a [`RunnerSetup`](crate::RunnerSetup) go to. The agent makes it
/// as it makes a host's: it creates a new one, or opens one that the test wrote first.
pub enum Start {
    /// A new session, which the agent creates in its storage (the setup's `config.storage`,
    /// in memory unless the test sets another) with these options.
    New(SessionOptions),
    /// The session `id`, which the test wrote into `storage`; the agent opens it there. A
    /// [`Seed`] converts into it.
    Open {
        /// Where the session is kept: the agent's storage.
        storage: Arc<dyn SessionStorage>,
        /// The session's ID.
        id: String,
    },
}

impl Default for Start {
    /// A new session that works in the current directory.
    fn default() -> Self {
        Start::New(SessionOptions::new("."))
    }
}

impl Start {
    /// A new session that works in the current directory and records to `log`, such as one
    /// that refuses records or one that hands them on; a [`LogStorage`] keeps it.
    pub fn with_log(log: impl SessionLog + 'static) -> Self {
        let header = header();
        let id = header.id.clone();
        Start::Open {
            storage: Arc::new(LogStorage::new(header, log)),
            id,
        }
    }
}

impl From<Seed> for Start {
    /// The session `seed` wrote, which the agent opens once the seed has let go of it.
    fn from(seed: Seed) -> Self {
        let Seed { storage, transcript } = seed;
        let id = transcript.header().id.clone();
        // A session has one writer: the seed's goes before the agent's comes.
        drop(transcript);
        Start::Open { storage, id }
    }
}

/// A session that a test writes before its runs, through the storage that keeps it, as the
/// runs of a host would have written it. It derefs to its [`Transcript`], to append to and
/// to read. Given to [`RunnerSetup::session`](crate::RunnerSetup::session) (with `into()`),
/// it lets go of the session, which the agent then opens in that storage.
pub struct Seed {
    storage: Arc<dyn SessionStorage>,
    transcript: Transcript,
}

impl Seed {
    /// A new session with `header`, kept in memory.
    pub async fn in_memory(header: SessionHeader) -> Self {
        Self::create(Arc::new(MemoryStorage::default()), header)
            .await
            .expect("a new storage in memory takes a session of the current format")
    }

    /// A new session with `header` in `storage`.
    async fn create(storage: Arc<dyn SessionStorage>, header: SessionHeader) -> anyhow::Result<Self> {
        let log = storage.create(&header).await?;
        Ok(Self {
            storage,
            transcript: Transcript::with_log(header, log),
        })
    }

    /// Append `entry` as one record line, through the storage that keeps the session, as a
    /// run that stopped right after writing it leaves it: for the records that only the kernel
    /// writes, such as the start of a tool call or of a compaction. The session is then read
    /// back from the storage, which checks the record as it checks every record of a session
    /// it opens; an error says why the storage or that check refused it.
    pub async fn append_record(&mut self, entry: Entry) -> anyhow::Result<()> {
        let header = self.transcript.header().clone();
        // The session's one writer lets go, so that the storage hands out its log again.
        drop(std::mem::replace(
            &mut self.transcript,
            Transcript::in_memory(header.clone()),
        ));
        let (mut loaded, mut log) = self.storage.open(&header.id).await?;
        let json = serde_json::to_string(&entry)?;
        let record = LogRecord {
            seq: u64::try_from(loaded.records.len())? + 1,
            id: entry.id(),
            parent_id: entry.parent_id(),
            kind: entry.kind(),
            timestamp: entry.timestamp(),
            json: &json,
        };
        log.append(&record).await?;
        loaded.records.push(json);
        self.transcript = Transcript::load(loaded, log)?;
        Ok(())
    }

    /// Append a message long enough to be worth summarizing, as an old turn of a long
    /// conversation; returns its entry ID, which a summary of it cites.
    pub async fn append_old_observation(&mut self) -> String {
        self.transcript
            .append_message(Message::user("observed ".repeat(600)))
            .await
            .expect("the session takes the message")
    }

    /// Record, after the session's last record, that the call `tool_call_id` to `tool_name`
    /// started, as [`append_record`](Self::append_record) does: what the kernel writes
    /// before it runs a call with side effects, and what a run that crashed while the call ran
    /// leaves behind.
    pub async fn record_tool_call_started(&mut self, tool_call_id: &str, tool_name: &str) -> anyhow::Result<()> {
        let entry = Entry::ToolCallStarted(ToolCallStartedEntry {
            id: format!("started-{tool_call_id}"),
            parent_id: self.transcript.leaf_id().map(String::from),
            timestamp: now_ms(),
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
        });
        self.append_record(entry).await
    }
}

impl Deref for Seed {
    type Target = Transcript;

    fn deref(&self) -> &Transcript {
        &self.transcript
    }
}

impl DerefMut for Seed {
    fn deref_mut(&mut self) -> &mut Transcript {
        &mut self.transcript
    }
}

/// A session in memory with one old message worth summarizing and a recent input; returns
/// the old message's entry ID.
pub async fn seeded_session() -> (Seed, String) {
    let mut seed = Seed::in_memory(header()).await;
    let old = seed.append_old_observation().await;
    seed.append_message(Message::user("most recent input")).await.unwrap();
    (seed, old)
}

/// A new session with `header` in the file `path`, through the storage of that one file.
pub async fn create_session_file(path: &Path, header: SessionHeader) -> anyhow::Result<Seed> {
    Seed::create(Arc::new(JsonlStorage::file(path)), header).await
}

/// The session in the file `path`, loaded and open for appending.
pub async fn open_session_file(path: &Path) -> anyhow::Result<Seed> {
    let id = read_session_file(path)?.header().id.clone();
    let storage = JsonlStorage::file(path);
    let (loaded, log) = storage.open(&id).await?;
    Ok(Seed {
        transcript: Transcript::load(loaded, log)?,
        storage: Arc::new(storage),
    })
}

/// A snapshot of the session in the file `path`: its header and complete lines, read here
/// and now, so that a callback, which cannot wait for a storage, can read it too.
pub fn read_session_file(path: &Path) -> anyhow::Result<Transcript> {
    let text = std::fs::read_to_string(path)?;
    let mut lines = text.lines();
    let header = serde_json::from_str(lines.next().unwrap_or_default())?;
    Transcript::snapshot(LoadedSession {
        header,
        records: lines.map(str::to_string).collect(),
        incomplete_tail_bytes: 0,
    })
}

/// A session log that refuses every record, like a database that no longer accepts writes:
/// a session on it closes itself once the operation that tried to write ends.
pub struct RefusingLog;

#[async_trait]
impl SessionLog for RefusingLog {
    async fn append(&mut self, _record: &LogRecord<'_>) -> anyhow::Result<()> {
        anyhow::bail!("the database is read-only")
    }
}

/// A storage of one session whose records go to a log the test gives, such as a
/// [`RefusingLog`] or a table the test reads back. The agent opens the session once, with the
/// records the test gave; the storage keeps nothing itself, so it lists no session and reads
/// none back.
pub struct LogStorage {
    loaded: LoadedSession,
    /// The session's log, until the session is opened.
    log: Mutex<Option<Box<dyn SessionLog>>>,
}

impl LogStorage {
    /// A storage of a session with `header` and no records yet, which records to `log`.
    fn new(header: SessionHeader, log: impl SessionLog + 'static) -> Self {
        let loaded = LoadedSession {
            header,
            records: Vec::new(),
            incomplete_tail_bytes: 0,
        };
        Self::holding(loaded, log)
    }

    /// A storage of the session `loaded`, which records to `log` from then on.
    pub fn holding(loaded: LoadedSession, log: impl SessionLog + 'static) -> Self {
        Self {
            loaded,
            log: Mutex::new(Some(Box::new(log))),
        }
    }
}

#[async_trait]
impl SessionStorage for LogStorage {
    async fn create(&self, header: &SessionHeader) -> anyhow::Result<Box<dyn SessionLog>> {
        anyhow::bail!(
            "session {} cannot be created: the storage holds session {} only",
            header.id,
            self.loaded.header.id
        )
    }

    async fn open(&self, id: &str) -> anyhow::Result<(LoadedSession, Box<dyn SessionLog>)> {
        anyhow::ensure!(id == self.loaded.header.id, "no session {id} in the storage");
        let log = self.log.lock().unwrap().take();
        let log = log.ok_or_else(|| anyhow::anyhow!("session {id} is open already: its log has one writer"))?;
        Ok((self.loaded.clone(), log))
    }

    async fn read(&self, id: &str) -> anyhow::Result<LoadedSession> {
        anyhow::bail!("the records of session {id} are in the test's log, not in the storage")
    }

    async fn list(&self, query: &SessionQuery) -> anyhow::Result<SessionList> {
        Ok(SessionList::new(query, Vec::new(), Vec::new()))
    }
}
