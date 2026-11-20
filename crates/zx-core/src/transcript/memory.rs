//! Sessions kept in memory: for a host that stores nothing, and for tests.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail, ensure};
use async_trait::async_trait;

use super::{
    LoadedSession, LogRecord, SessionHeader, SessionInfo, SessionList, SessionLog, SessionQuery, SessionStorage,
    Transcript, Unreadable,
};
use crate::sync::lock;

/// Sessions kept in memory, gone with the storage: what a host uses when it stores nothing,
/// such as for conversations that are not to outlast the process. Within the process a
/// session can still be listed, read, and opened again once its writer is dropped; a storage
/// made with [`MemoryStorage::forgetting`] drops the session then instead. Listing requires a
/// Tokio runtime: record replay runs on its blocking threads.
#[derive(Default)]
pub struct MemoryStorage {
    /// Shared with the logs of the sessions, which outlive any borrow of the storage.
    sessions: Arc<Mutex<HashMap<String, Stored>>>,
    /// What becomes of a session whose writer lets go.
    closed: Closed,
}

/// What a memory storage does with a session once its writer is dropped.
#[derive(Clone, Copy, Default)]
enum Closed {
    /// It stays, to be listed, read and opened again.
    #[default]
    Kept,
    /// It is gone, with its records.
    Forgotten,
}

struct Stored {
    header: SessionHeader,
    /// Every record's JSON, in `seq` order.
    records: Vec<String>,
    /// Whether a log of the session exists: its one writer.
    writing: bool,
}

impl Stored {
    fn loaded(&self) -> LoadedSession {
        LoadedSession {
            header: self.header.clone(),
            records: self.records.clone(),
            incomplete_tail_bytes: 0,
        }
    }
}

/// The writer of one session; dropping it lets the session be opened again, or forgets it.
struct MemoryLog {
    sessions: Arc<Mutex<HashMap<String, Stored>>>,
    id: String,
    closed: Closed,
}

impl MemoryStorage {
    /// A storage that keeps a session only while it is open: once its writer lets go, as when
    /// the agent closes the session, its records are dropped, and it is neither listed nor
    /// opened again. For a host that never opens a session a second time and runs for long,
    /// so that it does not keep every conversation it ever had.
    pub fn forgetting() -> Self {
        Self {
            sessions: Arc::default(),
            closed: Closed::Forgotten,
        }
    }

    fn log(&self, id: &str) -> Box<dyn SessionLog> {
        Box::new(MemoryLog {
            sessions: Arc::clone(&self.sessions),
            id: id.to_string(),
            closed: self.closed,
        })
    }
}

#[async_trait]
impl SessionStorage for MemoryStorage {
    async fn create(&self, header: &SessionHeader) -> Result<Box<dyn SessionLog>> {
        header.check_version()?;
        let mut sessions = lock(&self.sessions);
        ensure!(
            !sessions.contains_key(&header.id),
            "session {} exists already",
            header.id
        );
        sessions.insert(
            header.id.clone(),
            Stored {
                header: header.clone(),
                records: Vec::new(),
                writing: true,
            },
        );
        Ok(self.log(&header.id))
    }

    async fn open(&self, id: &str) -> Result<(LoadedSession, Box<dyn SessionLog>)> {
        let mut sessions = lock(&self.sessions);
        let stored = sessions.get_mut(id).ok_or_else(|| missing(id))?;
        ensure!(!stored.writing, "session {id} is in use: another writer has it open");
        stored.writing = true;
        Ok((stored.loaded(), self.log(id)))
    }

    async fn read(&self, id: &str) -> Result<LoadedSession> {
        let sessions = lock(&self.sessions);
        Ok(sessions.get(id).ok_or_else(|| missing(id))?.loaded())
    }

    async fn headers(&self, query: &SessionQuery) -> Result<Vec<SessionHeader>> {
        let headers = lock(&self.sessions)
            .values()
            .map(|stored| stored.header.clone())
            .collect();
        Ok(query.arrange_headers(headers))
    }

    async fn list(&self, query: &SessionQuery) -> Result<SessionList> {
        // Copied out under the lock, described without it.
        let matching: Vec<LoadedSession> = lock(&self.sessions)
            .values()
            .filter(|stored| query.matches(&stored.header))
            .map(Stored::loaded)
            .collect();
        let query = query.clone();
        tokio::task::spawn_blocking(move || {
            let mut sessions = Vec::new();
            let mut unreadable = Vec::new();
            for loaded in matching {
                let id = loaded.header.id.clone();
                match Transcript::snapshot(loaded) {
                    Ok(transcript) => sessions.push(SessionInfo::of(&transcript)),
                    Err(error) => unreadable.push(Unreadable {
                        error: format!("session {id}: {error:#}"),
                        location: id,
                    }),
                }
            }
            SessionList::new(&query, sessions, unreadable)
        })
        .await
        .context("session listing replay stopped")
    }
}

fn missing(id: &str) -> anyhow::Error {
    anyhow!("no session {id} in memory")
}

#[async_trait]
impl SessionLog for MemoryLog {
    async fn append(&mut self, record: &LogRecord<'_>) -> Result<()> {
        let mut sessions = lock(&self.sessions);
        let Some(stored) = sessions.get_mut(&self.id) else {
            bail!("session {} is no longer in memory", self.id);
        };
        let next = u64::try_from(stored.records.len())? + 1;
        record.check_seq(next).with_context(|| format!("session {}", self.id))?;
        stored.records.push(record.json.to_string());
        Ok(())
    }
}

impl Drop for MemoryLog {
    fn drop(&mut self) {
        let mut sessions = lock(&self.sessions);
        match self.closed {
            Closed::Kept => {
                if let Some(stored) = sessions.get_mut(&self.id) {
                    stored.writing = false;
                }
            }
            Closed::Forgotten => {
                sessions.remove(&self.id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::Message;
    use crate::transcript::test_support::{header, texts};

    #[tokio::test]
    async fn a_session_is_written_then_opened_again_with_its_records() {
        let storage = MemoryStorage::default();
        let header = header();
        let mut session = Transcript::with_log(header.clone(), storage.create(&header).await.unwrap());
        session.append_message(Message::user("q")).await.unwrap();
        drop(session);

        let (loaded, log) = storage.open(&header.id).await.unwrap();
        let mut reopened = Transcript::load(loaded, log).unwrap();
        reopened.append_message(Message::user("again")).await.unwrap();

        assert_eq!(texts(&reopened), ["q", "again"]);
        let snapshot = Transcript::snapshot(storage.read(&header.id).await.unwrap()).unwrap();
        assert_eq!(texts(&snapshot), ["q", "again"]);
    }

    #[tokio::test]
    async fn a_forgetting_storage_drops_a_session_when_its_writer_lets_go() {
        let storage = MemoryStorage::forgetting();
        let header = header();
        let mut session = Transcript::with_log(header.clone(), storage.create(&header).await.unwrap());
        session.append_message(Message::user("q")).await.unwrap();
        let listed = storage.list(&SessionQuery::default()).await.unwrap();
        assert_eq!(listed.sessions.len(), 1, "an open session is listed");

        drop(session);

        assert!(
            storage
                .list(&SessionQuery::default())
                .await
                .unwrap()
                .sessions
                .is_empty()
        );
        let error = storage.open(&header.id).await.err().unwrap().to_string();
        assert!(error.contains("no session"), "{error}");
        // Its ID is free again, as that of a session that never was.
        drop(storage.create(&header).await.unwrap());
    }

    #[tokio::test]
    async fn a_session_has_one_writer_at_a_time() {
        let storage = MemoryStorage::default();
        let header = header();
        let writer = storage.create(&header).await.unwrap();

        let error = storage.open(&header.id).await.err().unwrap().to_string();
        assert!(error.contains("is in use"), "{error}");
        assert!(storage.read(&header.id).await.is_ok(), "a snapshot needs no writer");

        drop(writer);
        assert!(storage.open(&header.id).await.is_ok());
    }

    #[tokio::test]
    async fn a_session_id_is_used_once_and_an_unknown_one_is_an_error() {
        let storage = MemoryStorage::default();
        let header = header();
        drop(storage.create(&header).await.unwrap());

        let error = storage.create(&header).await.err().unwrap().to_string();
        assert!(error.contains("exists already"), "{error}");
        for error in [
            storage.open("missing").await.err().unwrap(),
            storage.read("missing").await.err().unwrap(),
        ] {
            assert!(error.to_string().contains("no session missing"), "{error}");
        }
    }

    #[tokio::test]
    async fn a_record_out_of_order_is_refused() {
        let storage = MemoryStorage::default();
        let header = header();
        let mut log = storage.create(&header).await.unwrap();
        let record = |seq| LogRecord {
            seq,
            id: "a",
            parent_id: None,
            kind: "message",
            timestamp: 1,
            json: "{}",
        };

        log.append(&record(1)).await.unwrap();
        let error = format!("{:#}", log.append(&record(1)).await.unwrap_err());

        assert!(error.contains("expects record 2"), "{error}");
    }

    #[tokio::test]
    async fn a_listing_asks_by_label_and_names_the_latest_session_first() {
        let storage = MemoryStorage::default();
        let labelled = |value: &str, timestamp| SessionHeader {
            labels: [("chat".to_string(), value.to_string())].into(),
            timestamp,
            ..header()
        };
        let (older, newer, other) = (labelled("a", 1), labelled("a", 2), labelled("b", 3));
        for header in [&older, &newer, &other] {
            drop(storage.create(header).await.unwrap());
        }

        let query = SessionQuery {
            labels: [("chat".to_string(), "a".to_string())].into(),
            ..SessionQuery::default()
        };
        let listed = storage.list(&query).await.unwrap();

        let ids: Vec<&str> = listed.sessions.iter().map(|session| session.id.as_str()).collect();
        assert_eq!(ids, [newer.id.as_str(), older.id.as_str()]);
        assert_eq!(listed.sessions[0].labels, newer.labels);
        assert_eq!(storage.list(&SessionQuery::default()).await.unwrap().sessions.len(), 3);
    }

    #[tokio::test]
    async fn a_session_of_another_version_is_not_created() {
        let storage = MemoryStorage::default();
        let older = SessionHeader {
            version: crate::transcript::SESSION_VERSION - 1,
            ..header()
        };

        let error = storage.create(&older).await.err().unwrap().to_string();

        assert!(error.contains("is not supported"), "{error}");
        assert!(
            storage
                .list(&SessionQuery::default())
                .await
                .unwrap()
                .sessions
                .is_empty()
        );
    }
}
