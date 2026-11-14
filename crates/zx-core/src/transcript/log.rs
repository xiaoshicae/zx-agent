//! Where one session's records are kept. The store validates and orders them; a log only
//! keeps them, in order, and its storage gives them back when the session is opened again.

use anyhow::Result;
use async_trait::async_trait;

use super::SessionHeader;

/// The records of one session, appended in order after its header: what a
/// [`SessionStorage`](super::SessionStorage) hands out for a session it created or opened. A
/// session file is one implementation; the rows of a database table are another.
///
/// [`Transcript`](super::Transcript) checks each record before it reaches the log,
/// waits for the log before changing memory, and stops writing after the first failure,
/// including an append that timed out or whose future was dropped, because its outcome is
/// unknown. A log only has to keep the records in order, so that its storage can return them
/// as a [`LoadedSession`] when the session is opened again.
///
/// # Example
///
/// A log that hands each record on, as an insert into a database table would. A real
/// backend also stores the header when it creates the session, and rejects a second record
/// with the same `seq`.
///
/// ```
/// use std::path::Path;
/// use std::sync::mpsc;
///
/// use async_trait::async_trait;
/// use zx_ai::Message;
/// use zx_core::transcript::{LoadedSession, LogRecord, SessionHeader, SessionLog, Transcript};
///
/// struct Table(mpsc::Sender<String>);
///
/// #[async_trait]
/// impl SessionLog for Table {
///     async fn append(&mut self, record: &LogRecord<'_>) -> anyhow::Result<()> {
///         self.0.send(record.json.to_string())?;
///         Ok(())
///     }
/// }
///
/// # #[tokio::main]
/// # async fn main() -> anyhow::Result<()> {
/// let header = SessionHeader::new(Path::new("."), "model");
/// let (rows, table) = mpsc::channel();
/// let mut store = Transcript::with_log(header.clone(), Box::new(Table(rows)));
/// store.append_message(Message::user("check the disk")).await?;
///
/// // Later, possibly in another process: read the rows back and continue the session.
/// let loaded = LoadedSession {
///     header,
///     records: table.try_iter().collect(),
///     incomplete_tail_bytes: 0,
/// };
/// let (rows, _table) = mpsc::channel();
/// let reopened = Transcript::load(loaded, Box::new(Table(rows)))?;
/// assert_eq!(reopened.messages(), store.messages());
/// # Ok(())
/// # }
/// ```
#[async_trait]
pub trait SessionLog: Send + Sync {
    /// Store `record` after every earlier one, and return only once it is stored. Must fail,
    /// rather than overwrite, when a record with the same `seq` already exists: two writers
    /// would otherwise fork the session. A database can enforce this with a unique key on
    /// the session ID and `seq`.
    async fn append(&mut self, record: &LogRecord<'_>) -> Result<()>;
}

/// One record of a session, as the store hands it to the log.
#[derive(Clone, Copy, Debug)]
pub struct LogRecord<'a> {
    /// Position after the header, starting at 1.
    pub seq: u64,
    /// The entry's ID.
    pub id: &'a str,
    /// The parent entry's ID; `None` for a root entry.
    pub parent_id: Option<&'a str>,
    /// The entry type, such as `message` or `context_edit`.
    pub kind: &'a str,
    /// Milliseconds since the Unix epoch.
    pub timestamp: u64,
    /// The entry as JSON, exactly as a JSONL session file stores it on one line. The other
    /// fields repeat parts of it, so that a database can index them.
    pub json: &'a str,
}

/// A stored session as its storage reads it back, for
/// [`Transcript::load`](super::Transcript::load) and
/// [`Transcript::snapshot`](super::Transcript::snapshot).
#[derive(Clone, Debug)]
pub struct LoadedSession {
    pub header: SessionHeader,
    /// Every record's [`LogRecord::json`], in `seq` order.
    pub records: Vec<String>,
    /// Bytes of an incomplete final record that the backend left out, such as a line that a
    /// crash cut short; zero when there was none.
    pub incomplete_tail_bytes: usize,
}
