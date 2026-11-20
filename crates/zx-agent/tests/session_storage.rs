//! A session storage of the host's own, through the public SDK: what a storage backed by a
//! database implements, and that an agent creates its sessions in it, lists them from it and
//! continues them from it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use zx_agent::ai::Message;
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::events::RunOutcome;
use zx_agent::transcript::{
    LoadedSession, LogRecord, SessionHeader, SessionInfo, SessionList, SessionLog, SessionQuery, SessionStorage,
    Transcript,
};
use zx_agent::{Agent, AgentConfig, SessionOptions};
use zx_testing::{config, header, summarize};

/// Two tables, as a database would hold them: the sessions by ID, and their records under
/// the unique key of session ID and `seq`.
#[derive(Default)]
struct Tables {
    sessions: Mutex<BTreeMap<String, SessionHeader>>,
    records: Mutex<BTreeMap<(String, u64), String>>,
}

#[derive(Default)]
struct TableStorage(Arc<Tables>);

struct TableStorageLog {
    tables: Arc<Tables>,
    id: String,
}

impl TableStorage {
    fn log(&self, id: &str) -> Box<dyn SessionLog> {
        Box::new(TableStorageLog {
            tables: Arc::clone(&self.0),
            id: id.to_string(),
        })
    }
}

#[async_trait]
impl SessionLog for TableStorageLog {
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
        Ok(SessionList::new(query, sessions, Vec::new()))
    }
}

fn labels(tenant: &str) -> BTreeMap<String, String> {
    BTreeMap::from([("tenant".to_string(), tenant.to_string())])
}

fn of_tenant(tenant: &str) -> SessionQuery {
    SessionQuery {
        labels: labels(tenant),
        ..SessionQuery::default()
    }
}

/// A new session of `tenant` in the current directory.
fn for_tenant(tenant: &str) -> SessionOptions {
    SessionOptions {
        labels: labels(tenant),
        ..SessionOptions::new(".")
    }
}

/// An agent of `provider` that keeps its sessions in `storage`.
fn agent(provider: &Arc<FauxProvider>, storage: &Arc<TableStorage>) -> Agent {
    Agent::new(AgentConfig {
        storage: storage.clone(),
        ..config(provider.clone(), vec![])
    })
    .unwrap()
}

#[tokio::test]
async fn an_agent_keeps_its_sessions_in_a_custom_storage_and_a_later_agent_continues_them() {
    let storage = Arc::new(TableStorage::default());
    let provider = Arc::new(FauxProvider::new([FauxStep::text("one"), FauxStep::text("two")]));
    let first = agent(&provider, &storage);
    let session = first.create_session(for_tenant("acme")).await.unwrap();
    let report = session.send("first").unwrap().wait().await;
    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    assert_eq!(report.session_id, session.id());
    let id = session.id().to_string();
    first.create_session(for_tenant("other")).await.unwrap();
    drop((first, session));

    // Another process, as far as the kernel can tell: all it has is the storage.
    let second = agent(&provider, &storage);
    let listed = second.list_sessions(&of_tenant("acme")).await.unwrap();
    assert_eq!(listed.sessions.len(), 1, "{listed:?}");
    let found = &listed.sessions[0];
    assert_eq!(
        (found.id.as_str(), found.messages, found.first_prompt.as_deref()),
        (id.as_str(), 2, Some("first"))
    );
    assert_eq!(found.labels, labels("acme"));
    let session = second.open_session(&found.id).await.unwrap();
    let report = session.send("second").unwrap().wait().await;

    assert_eq!(report.final_text, "two");
    assert_eq!(
        summarize(&provider.requests()[1].messages),
        ["user:first", "assistant:one", "user:second"]
    );
    let mut snapshot = Transcript::snapshot(storage.read(&id).await.unwrap()).unwrap();
    assert_eq!(snapshot.messages().len(), 4);
    let error = snapshot.append_message(Message::user("more")).await.unwrap_err();
    assert!(
        error.to_string().contains("read as a snapshot, which is read-only"),
        "{error}"
    );
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
    let later = second.append_message(Message::user("again")).await.unwrap_err();
    assert!(later.to_string().contains("an earlier write failed"), "{later}");
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
