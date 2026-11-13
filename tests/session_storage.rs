//! A session storage of the host's own, through the public SDK: what a storage backed by a
//! database implements, and that a run records in it, is listed from it and continues from
//! it.
mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use common::{RunnerSetup, config, header, summarize};
use zx_agent::ai::Message;
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::runner::RunOutcome;
use zx_agent::transcript::{
    LoadedSession, LogRecord, SessionHeader, SessionInfo, SessionList, SessionLog, SessionQuery, SessionStorage,
    Transcript, WritesStopped,
};

/// Two tables, as a database would hold them: the sessions by ID, and their records under
/// the unique key of session ID and `seq`.
#[derive(Default)]
struct Tables {
    sessions: Mutex<BTreeMap<String, SessionHeader>>,
    records: Mutex<BTreeMap<(String, u64), String>>,
}

#[derive(Default)]
struct TableStorage(Arc<Tables>);

struct TableLog {
    tables: Arc<Tables>,
    id: String,
}

impl TableStorage {
    fn log(&self, id: &str) -> Box<dyn SessionLog> {
        Box::new(TableLog {
            tables: Arc::clone(&self.0),
            id: id.to_string(),
        })
    }
}

#[async_trait]
impl SessionLog for TableLog {
    async fn append(&mut self, record: &LogRecord<'_>) -> Result<()> {
        let mut records = self.tables.records.lock().unwrap();
        let key = (self.id.clone(), record.seq);
        // The unique key: where a record is already, a second writer's is refused.
        ensure!(
            !records.contains_key(&key),
            "duplicate key ({}, {})",
            self.id,
            record.seq
        );
        records.insert(key, record.json.to_string());
        Ok(())
    }
}

#[async_trait]
impl SessionStorage for TableStorage {
    async fn create(&self, header: &SessionHeader) -> Result<Box<dyn SessionLog>> {
        let mut sessions = self.0.sessions.lock().unwrap();
        ensure!(
            !sessions.contains_key(&header.id),
            "session {} exists already",
            header.id
        );
        sessions.insert(header.id.clone(), header.clone());
        Ok(self.log(&header.id))
    }

    async fn open(&self, id: &str) -> Result<(LoadedSession, Box<dyn SessionLog>)> {
        Ok((self.read(id).await?, self.log(id)))
    }

    async fn read(&self, id: &str) -> Result<LoadedSession> {
        let sessions = self.0.sessions.lock().unwrap();
        let header = sessions.get(id).with_context(|| format!("no session {id}"))?;
        let records = self.0.records.lock().unwrap();
        let of_session = (id.to_string(), u64::MIN)..=(id.to_string(), u64::MAX);
        Ok(LoadedSession {
            header: header.clone(),
            records: records.range(of_session).map(|(_, json)| json.clone()).collect(),
            incomplete_tail_bytes: 0,
        })
    }

    async fn list(&self, query: &SessionQuery) -> Result<SessionList> {
        let matching: Vec<String> = {
            let sessions = self.0.sessions.lock().unwrap();
            let asked_for = sessions.values().filter(|header| query.matches(header));
            asked_for.map(|header| header.id.clone()).collect()
        };
        let mut sessions = Vec::new();
        for id in matching {
            sessions.push(SessionInfo::of(&Transcript::snapshot(self.read(&id).await?)?));
        }
        Ok(SessionList {
            sessions: query.arrange(sessions),
            unreadable: Vec::new(),
        })
    }
}

fn labelled(tenant: &str) -> SessionHeader {
    SessionHeader {
        labels: BTreeMap::from([("tenant".to_string(), tenant.to_string())]),
        ..header()
    }
}

fn of_tenant(tenant: &str) -> SessionQuery {
    SessionQuery {
        labels: BTreeMap::from([("tenant".to_string(), tenant.to_string())]),
        ..SessionQuery::default()
    }
}

#[tokio::test]
async fn a_run_records_in_a_custom_storage_and_a_later_run_continues_from_it() {
    let storage = TableStorage::default();
    let provider = Arc::new(FauxProvider::new([FauxStep::text("one"), FauxStep::text("two")]));
    let header = labelled("acme");
    let session = Transcript::with_log(header.clone(), storage.create(&header).await.unwrap());
    let (mut runner, _) = RunnerSetup {
        session,
        ..RunnerSetup::new(config(provider.clone(), vec![]))
    }
    .build();
    let report = runner.run("first", &CancellationToken::new()).await;
    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    assert_eq!(report.session_id, header.id);
    drop(runner);
    drop(storage.create(&labelled("other")).await.unwrap());

    // Another process, as far as the kernel can tell: all it has is the storage.
    let listed = storage.list(&of_tenant("acme")).await.unwrap();
    assert_eq!(listed.sessions.len(), 1, "{listed:?}");
    let found = &listed.sessions[0];
    assert_eq!(
        (found.id.as_str(), found.messages, found.first_prompt.as_deref()),
        (header.id.as_str(), 2, Some("first"))
    );
    assert_eq!(found.labels, header.labels);
    let (loaded, log) = storage.open(&found.id).await.unwrap();
    let (mut runner, _) = RunnerSetup {
        session: Transcript::load(loaded, log).unwrap(),
        ..RunnerSetup::new(config(provider.clone(), vec![]))
    }
    .build();
    let report = runner.run("second", &CancellationToken::new()).await;

    assert_eq!(report.final_text, "two");
    assert_eq!(
        summarize(&provider.requests()[1].messages),
        ["user:first", "assistant:one", "user:second"]
    );
    let snapshot = Transcript::snapshot(storage.read(&header.id).await.unwrap()).unwrap();
    assert_eq!(snapshot.messages().len(), 4);
    assert_eq!(snapshot.writes_stopped(), Some(&WritesStopped::ReadOnly));
}

#[tokio::test]
async fn the_unique_key_stops_a_second_writer_of_the_same_session() {
    let storage = TableStorage::default();
    let header = header();
    drop(storage.create(&header).await.unwrap());
    let open = async || {
        let (loaded, log) = storage.open(&header.id).await.unwrap();
        Transcript::load(loaded, log).unwrap()
    };
    let (mut first, mut second) = (open().await, open().await);

    first.append_message(Message::user("mine")).await.unwrap();
    let error = second.append_message(Message::user("theirs")).await.unwrap_err();

    assert!(format!("{error:#}").contains("duplicate key"), "{error:#}");
    assert!(matches!(second.writes_stopped(), Some(WritesStopped::Failed(_))));
    assert!(second.messages().is_empty(), "a refused record is not in memory either");
    let stored = Transcript::snapshot(storage.read(&header.id).await.unwrap()).unwrap();
    assert_eq!(summarize(&stored.messages()), ["user:mine"]);
}

#[tokio::test]
async fn a_session_id_is_created_once_and_an_unknown_one_cannot_be_opened() {
    let storage = TableStorage::default();
    let header = header();
    drop(storage.create(&header).await.unwrap());

    let error = storage.create(&header).await.err().unwrap();
    assert!(error.to_string().contains("exists already"), "{error}");
    let error = storage.open("missing").await.err().unwrap();
    assert!(error.to_string().contains("no session missing"), "{error}");
    assert!(storage.list(&of_tenant("acme")).await.unwrap().sessions.is_empty());
}
