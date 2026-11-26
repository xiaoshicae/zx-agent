//! The sessions of the conversations: a header per session in `zx_im_sessions`, a row per
//! record in `zx_im_records`, keyed by the session and its `seq`, so that a second writer's
//! append fails rather than forks the session.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use tokio_postgres::Row;
use zx_agent::transcript::{
    LoadedSession, LogRecord, SessionHeader, SessionInfo, SessionList, SessionLog, SessionQuery, SessionStorage,
    Transcript, Unreadable,
};

use super::{Database, Failure, bigint};

/// The sessions, kept in the database.
pub(crate) struct PostgresSessions {
    database: Arc<Database>,
}

/// The writer of one session.
struct PostgresLog {
    database: Arc<Database>,
    id: String,
}

impl PostgresSessions {
    pub(crate) fn new(database: Arc<Database>) -> Self {
        Self { database }
    }

    fn log(&self, id: &str) -> Box<dyn SessionLog> {
        Box::new(PostgresLog {
            database: Arc::clone(&self.database),
            id: id.to_string(),
        })
    }

    /// The session `id` as kept: its header and its records in order.
    async fn load(&self, id: &str) -> Result<LoadedSession> {
        let failed = |failure: Failure| anyhow!("cannot read session {id}: {failure}");
        let headers = self
            .database
            .query("SELECT header FROM zx_im_sessions WHERE id = $1", &[&id])
            .await
            .map_err(failed)?;
        let row = headers.first().ok_or_else(|| anyhow!("no session {id}"))?;
        let header = header_of(row)?;
        let rows = self
            .database
            .query(
                "SELECT record FROM zx_im_records WHERE session_id = $1 ORDER BY seq",
                &[&id],
            )
            .await
            .map_err(failed)?;
        let records = rows
            .iter()
            .map(|row| row.try_get(0))
            .collect::<Result<Vec<String>, _>>()
            .with_context(|| format!("cannot read the records of session {id}"))?;
        Ok(LoadedSession {
            header,
            records,
            incomplete_tail_bytes: 0,
        })
    }

    /// The IDs and headers of the sessions `query` asks for, whatever its limit.
    async fn matching(&self, query: &SessionQuery) -> Result<Vec<Row>> {
        let cwd = query.cwd.as_ref().map(|cwd| cwd.display().to_string());
        let labels = serde_json::to_string(&query.labels)?;
        let rows = self
            .database
            .query(
                "SELECT id, header FROM zx_im_sessions \
                 WHERE ($1::text IS NULL OR cwd = $1) AND labels @> $2::text::jsonb",
                &[&cwd, &labels],
            )
            .await
            .map_err(|failure| anyhow!("cannot list the sessions: {failure}"))?;
        Ok(rows)
    }
}

#[async_trait]
impl SessionStorage for PostgresSessions {
    async fn create(&self, header: &SessionHeader) -> Result<Box<dyn SessionLog>> {
        header.check_version()?;
        let text = serde_json::to_string(header)?;
        let labels = serde_json::to_string(&header.labels)?;
        let created = self
            .database
            .write(
                "INSERT INTO zx_im_sessions (id, header, cwd, labels) \
                 SELECT $1::text, $2::text, $3::text, $4::text::jsonb WHERE zx_im_fence($5)",
                &[&header.id, &text, &header.cwd, &labels],
            )
            .await;
        match created {
            Ok(_) => Ok(self.log(&header.id)),
            Err(Failure::Duplicate) => bail!("session {} exists already", header.id),
            Err(failure) => bail!("cannot create session {}: {failure}", header.id),
        }
    }

    /// A second writer opens it too: its appends fail on the records the first wrote.
    async fn open(&self, id: &str) -> Result<(LoadedSession, Box<dyn SessionLog>)> {
        Ok((self.load(id).await?, self.log(id)))
    }

    async fn read(&self, id: &str) -> Result<LoadedSession> {
        self.load(id).await
    }

    async fn headers(&self, query: &SessionQuery) -> Result<Vec<SessionHeader>> {
        let rows = self.matching(query).await?;
        // A header that cannot be read is left out; a listing reports it.
        let headers = rows.iter().filter_map(|row| header_of_column(row, 1).ok()).collect();
        Ok(query.arrange_headers(headers))
    }

    async fn list(&self, query: &SessionQuery) -> Result<SessionList> {
        let rows = self.matching(query).await?;
        let mut loaded = Vec::with_capacity(rows.len());
        let mut unreadable = Vec::new();
        for row in &rows {
            let id: String = row.try_get(0).context("cannot read a session's ID")?;
            match self.load(&id).await {
                Ok(session) => loaded.push(session),
                Err(error) => unreadable.push(Unreadable {
                    error: format!("session {id}: {error:#}"),
                    location: id,
                }),
            }
        }
        let query = query.clone();
        // Replayed off the runtime, as the other storages do.
        tokio::task::spawn_blocking(move || {
            let mut sessions = Vec::with_capacity(loaded.len());
            for session in loaded {
                let id = session.header.id.clone();
                match Transcript::snapshot(session) {
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

#[async_trait]
impl SessionLog for PostgresLog {
    async fn append(&mut self, record: &LogRecord<'_>) -> Result<()> {
        let seq = bigint(record.seq, "the record number").map_err(|failure| anyhow!("{failure}"))?;
        let appended = self
            .database
            .write(
                "INSERT INTO zx_im_records (session_id, seq, record) \
                 SELECT $1::text, $2::bigint, $3::text WHERE zx_im_fence($4)",
                &[&self.id, &seq, &record.json],
            )
            .await;
        match appended {
            Ok(_) => Ok(()),
            Err(Failure::Duplicate) => bail!(
                "session {} has a record {} already: another writer has it open",
                self.id,
                record.seq
            ),
            Err(failure) => bail!("cannot append to session {}: {failure}", self.id),
        }
    }
}

/// The header in the first column of `row`.
fn header_of(row: &Row) -> Result<SessionHeader> {
    header_of_column(row, 0)
}

/// The header in the column `column` of `row`.
fn header_of_column(row: &Row, column: usize) -> Result<SessionHeader> {
    let text: String = row.try_get(column).context("cannot read a session's header")?;
    serde_json::from_str(&text).context("invalid session header")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use zx_agent::ai::Message;
    use zx_agent::transcript::SESSION_VERSION;

    use super::super::test_support::leading;
    use super::*;

    /// A header of a session in `cwd` with the label `who`.
    fn header(cwd: &str, who: &str) -> SessionHeader {
        let mut header = SessionHeader::new(Path::new(cwd), "model");
        header.labels = BTreeMap::from([("who".to_string(), who.to_string())]);
        header
    }

    #[tokio::test]
    async fn a_session_is_written_found_by_its_labels_and_opened_again() {
        let Some(database) = leading().await else {
            return;
        };
        let storage = PostgresSessions::new(database);
        let header = header("/work", "ops");
        let id = header.id.clone();
        let log = storage.create(&header).await.unwrap();
        let mut session = Transcript::with_log(header.clone(), log);
        session.append_message(Message::user("check the disk")).await.unwrap();
        drop(session);
        storage.create(&self::header("/work", "dev")).await.unwrap();

        let (loaded, log) = storage.open(&id).await.unwrap();
        let mut continued = Transcript::load(loaded, log).unwrap();
        assert_eq!(continued.messages().len(), 1);
        continued.append_message(Message::user("and the memory")).await.unwrap();
        assert_eq!(storage.read(&id).await.unwrap().records.len(), 2);

        let ops = SessionQuery {
            labels: BTreeMap::from([("who".to_string(), "ops".to_string())]),
            ..SessionQuery::default()
        };
        assert_eq!(storage.headers(&ops).await.unwrap(), [header]);
        let listed = storage.list(&ops).await.unwrap();
        assert_eq!(listed.sessions.len(), 1);
        assert_eq!(listed.sessions[0].id, id);
        let elsewhere = SessionQuery {
            cwd: Some("/other".into()),
            ..SessionQuery::default()
        };
        assert!(storage.headers(&elsewhere).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_session_of_another_format_is_left_out_of_the_headers_and_reported_by_a_listing() {
        let Some(database) = leading().await else {
            return;
        };
        let storage = PostgresSessions::new(Arc::clone(&database));
        let current = header("/work", "ops");
        storage.create(&current).await.unwrap();
        let older = header("/work", "ops");
        storage.create(&older).await.unwrap();
        // As an earlier program wrote it: `create` refuses a header of another format.
        let text = serde_json::to_string(&SessionHeader {
            version: SESSION_VERSION - 1,
            ..older.clone()
        })
        .unwrap();
        let changed = database
            .write(
                "UPDATE zx_im_sessions SET header = $2::text WHERE id = $1::text AND zx_im_fence($3)",
                &[&older.id, &text],
            )
            .await
            .unwrap();
        assert_eq!(changed, 1);

        assert_eq!(storage.headers(&SessionQuery::default()).await.unwrap(), [current]);
        let listed = storage.list(&SessionQuery::default()).await.unwrap();
        assert_eq!(listed.unreadable.len(), 1, "{:?}", listed.unreadable);
        assert_eq!(listed.unreadable[0].location, older.id);
    }

    #[tokio::test]
    async fn a_session_is_created_once_and_a_second_writer_cannot_fork_it() {
        let Some(database) = leading().await else {
            return;
        };
        let storage = PostgresSessions::new(database);
        let header = header("/work", "ops");
        let log = storage.create(&header).await.unwrap();
        let error = storage.create(&header).await.err().unwrap();
        assert!(format!("{error:#}").contains("exists already"), "{error:#}");

        let mut first = Transcript::with_log(header.clone(), log);
        let (loaded, log) = storage.open(&header.id).await.unwrap();
        let mut second = Transcript::load(loaded, log).unwrap();
        first.append_message(Message::user("one")).await.unwrap();
        let error = second.append_message(Message::user("two")).await.unwrap_err();
        assert!(format!("{error:#}").contains("another writer has it open"), "{error:#}");
        assert!(storage.read("missing").await.is_err());
    }
}
