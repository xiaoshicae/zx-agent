use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};
use tokio::sync::Notify;

use super::*;
use crate::ai::faux::{FauxProvider, FauxStep};
use crate::ai::{Message, ToolDef};
use crate::runner::RunOutcome;
use crate::session::SessionError;
use crate::tools::{ToolContext, ToolOutput};
use crate::transcript::{LoadedSession, LogRecord, SessionList, SessionLog};

fn cwd() -> &'static Path {
    Path::new(".")
}

/// An agent that answers with `steps`, and its storage and provider.
fn agent(steps: impl IntoIterator<Item = FauxStep>) -> (Agent, Arc<MemoryStorage>, Arc<FauxProvider>) {
    let provider = Arc::new(FauxProvider::new(steps));
    let storage = Arc::new(MemoryStorage::default());
    let agent = Agent::new(AgentConfig {
        storage: storage.clone(),
        ..AgentConfig::new(provider.clone(), "m")
    })
    .unwrap();
    (agent, storage, provider)
}

/// The IDs of the sessions `storage` keeps.
async fn kept(storage: &MemoryStorage) -> Vec<String> {
    let list = storage.list(&SessionQuery::default()).await.unwrap();
    list.sessions.into_iter().map(|session| session.id).collect()
}

/// A tool called by the name it is made with, which answers "ran".
struct Named(&'static str);

#[async_trait]
impl Tool for Named {
    fn name(&self) -> &str {
        self.0
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: self.0.to_string(),
            description: "Answers".to_string(),
            parameters: json!({"type": "object"}),
        }
    }

    async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, String> {
        Ok(ToolOutput::ok("ran"))
    }
}

/// Gives each session a tool of its own and a prompt section that names its working
/// directory, counts the sessions it set up, waits at `gate` when it has one, and fails
/// with `fail` when it has one.
#[derive(Default)]
struct Setup {
    calls: AtomicUsize,
    gate: Option<Arc<Notify>>,
    fail: Option<&'static str>,
}

#[async_trait]
impl SessionSetup for Setup {
    async fn setup(&self, header: &SessionHeader) -> Result<SessionParts> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.gate {
            gate.notified().await;
        }
        if let Some(reason) = self.fail {
            anyhow::bail!(reason);
        }
        Ok(SessionParts {
            tools: vec![Arc::new(Named("own"))],
            prompt_sections: vec![("place".into(), header.cwd.clone())],
            ..SessionParts::default()
        })
    }
}

/// An agent with the tool `shared` and with `setup`, and its storage and provider.
fn agent_with(
    setup: Arc<Setup>,
    steps: impl IntoIterator<Item = FauxStep>,
) -> (Agent, Arc<MemoryStorage>, Arc<FauxProvider>) {
    let provider = Arc::new(FauxProvider::new(steps));
    let storage = Arc::new(MemoryStorage::default());
    let agent = Agent::new(AgentConfig {
        tools: vec![Arc::new(Named("shared"))],
        storage: storage.clone(),
        session_setup: Some(setup),
        ..AgentConfig::new(provider.clone(), "m")
    })
    .unwrap();
    (agent, storage, provider)
}

#[tokio::test]
async fn a_created_session_runs_and_is_kept_in_the_storage_with_its_labels() {
    let (agent, storage, _) = agent([FauxStep::text("done")]);
    let options = SessionOptions {
        labels: [("tenant".to_string(), "a".to_string())].into(),
        ..SessionOptions::new("/work")
    };

    let session = agent.create_session(options).await.unwrap();
    let report = session.send("go").unwrap().wait().await;

    assert_eq!(
        (report.outcome, report.final_text.as_str()),
        (RunOutcome::Completed, "done")
    );
    let list = storage.list(&SessionQuery::default()).await.unwrap();
    let [stored] = list.sessions.as_slice() else {
        panic!("one session is kept: {:?}", list.sessions);
    };
    assert_eq!(
        (stored.id.as_str(), stored.cwd.as_str(), stored.messages),
        (session.id(), "/work", 2)
    );
    assert_eq!(stored.labels.get("tenant").map(String::as_str), Some("a"));
}

#[tokio::test]
async fn an_open_session_is_found_and_opened_as_the_same_one() {
    let (agent, _, _) = agent([FauxStep::Hang]);
    let session = agent.create_session(SessionOptions::new(cwd())).await.unwrap();

    let found = agent.session(session.id()).unwrap();
    let opened = agent.open_session(session.id()).await.unwrap();

    // One conversation: a run on one handle makes the others busy.
    let run = session.send("go").unwrap();
    assert_eq!(found.send("too").err(), Some(SessionError::Busy));
    assert_eq!(opened.send("too").err(), Some(SessionError::Busy));
    drop(run);
    assert!(agent.session("another").is_none());
}

#[tokio::test]
async fn a_closed_session_refuses_its_handles_and_opens_again_from_the_storage() {
    let (agent, _, _) = agent([FauxStep::text("one"), FauxStep::text("two")]);
    let session = agent.create_session(SessionOptions::new(cwd())).await.unwrap();
    session.send("first").unwrap().wait().await;
    let id = session.id().to_string();

    assert!(agent.close_session(&id));

    assert_eq!(session.send("again").err(), Some(SessionError::Closed));
    assert_eq!(session.with_transcript(|_| ()).err(), Some(SessionError::Closed));
    assert!(!session.cancel());
    assert!(agent.session(&id).is_none());
    assert!(!agent.close_session(&id), "closed already");
    let reopened = agent.open_session(&id).await.unwrap();
    let report = reopened.send("second").unwrap().wait().await;
    assert_eq!(report.final_text, "two");
    let messages = reopened
        .with_transcript(|transcript| transcript.messages().len())
        .unwrap();
    assert_eq!(messages, 4);
}

/// Keeps sessions in memory, but the log of a session it creates never answers: what a
/// caller meets who drops a run while a record is on its way to the storage. A session
/// opened again gets a log that works.
#[derive(Default)]
struct Stalling {
    kept: MemoryStorage,
    /// Notified when a record is handed to a log that will not answer.
    writing: Arc<Notify>,
}

struct StallingLog(Arc<Notify>);

#[async_trait]
impl SessionLog for StallingLog {
    async fn append(&mut self, _record: &LogRecord<'_>) -> Result<()> {
        self.0.notify_one();
        std::future::pending().await
    }
}

#[async_trait]
impl SessionStorage for Stalling {
    async fn create(&self, header: &SessionHeader) -> Result<Box<dyn SessionLog>> {
        // The session is kept, without a writer of its own, so that it can be opened again.
        drop(self.kept.create(header).await?);
        Ok(Box::new(StallingLog(self.writing.clone())))
    }

    async fn open(&self, id: &str) -> Result<(LoadedSession, Box<dyn SessionLog>)> {
        self.kept.open(id).await
    }

    async fn read(&self, id: &str) -> Result<LoadedSession> {
        self.kept.read(id).await
    }

    async fn list(&self, query: &SessionQuery) -> Result<SessionList> {
        self.kept.list(query).await
    }
}

#[tokio::test]
async fn a_run_dropped_while_a_record_is_written_closes_the_session_which_opens_again() {
    let storage = Arc::new(Stalling::default());
    let agent = Agent::new(AgentConfig {
        storage: storage.clone(),
        ..AgentConfig::new(Arc::new(FauxProvider::new([FauxStep::text("answer")])), "m")
    })
    .unwrap();
    let session = agent.create_session(SessionOptions::new(cwd())).await.unwrap();
    let id = session.id().to_string();
    let mut run = session.send("first").unwrap();
    let writing = storage.writing.notified();
    tokio::pin!(writing);
    loop {
        tokio::select! {
            () = &mut writing => break,
            event = run.next() => assert!(event.is_some(), "the run ended before it recorded anything"),
        }
    }

    // Whether the storage got the record is unknown from here on.
    drop(run);

    assert_eq!(session.send("again").err(), Some(SessionError::Closed));
    assert!(agent.session(&id).is_none());
    let reopened = agent.open_session(&id).await.unwrap();
    let report = reopened.send("again").unwrap().wait().await;
    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    assert_eq!(report.final_text, "answer");
}

#[tokio::test]
async fn closing_a_session_cancels_its_run_and_frees_it_once_the_run_ended() {
    let (agent, _, _) = agent([FauxStep::Hang]);
    let session = agent.create_session(SessionOptions::new(cwd())).await.unwrap();
    let id = session.id().to_string();
    let run = session.send("go").unwrap();

    assert!(agent.close_session(&id));
    let report = run.wait().await;

    assert_eq!(report.outcome, RunOutcome::Cancelled);
    assert_eq!(session.send("again").err(), Some(SessionError::Closed));
    // The storage has its one writer back.
    agent.open_session(&id).await.unwrap();
}

#[tokio::test]
async fn a_session_runs_with_the_parts_its_setup_makes() {
    let setup = Arc::new(Setup::default());
    let (agent, _, provider) = agent_with(setup.clone(), [FauxStep::text("done")]);

    let session = agent.create_session(SessionOptions::new("/work")).await.unwrap();
    session.send("go").unwrap().wait().await;

    assert_eq!(session.tool_names(), ["shared", "own"]);
    let request = &provider.requests()[0];
    let declared: Vec<&str> = request.tools.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(declared, ["shared", "own"]);
    let Some(Message::System(system)) = request.messages.first() else {
        panic!("the request starts with the system prompt: {:?}", request.messages);
    };
    let sections: Vec<(&str, Option<&str>)> = system
        .sections
        .iter()
        .map(|section| (section.name.as_str(), section.content.as_deref()))
        .collect();
    assert_eq!(sections, [("place", Some("/work"))]);
    assert_eq!(setup.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_setup_that_fails_leaves_no_session_in_the_storage() {
    let setup = Arc::new(Setup {
        fail: Some("no container"),
        ..Setup::default()
    });
    let (agent, storage, _) = agent_with(setup, []);

    let error = agent.create_session(SessionOptions::new(cwd())).await.err().unwrap();

    assert_eq!(format!("{error:#}"), "cannot set up the session: no container");
    assert!(kept(&storage).await.is_empty());
}

#[tokio::test]
async fn opening_one_session_twice_at_once_gives_the_same_session() {
    let gate = Arc::new(Notify::new());
    let setup = Arc::new(Setup {
        gate: Some(gate.clone()),
        ..Setup::default()
    });
    let (agent, storage, _) = agent_with(setup.clone(), [FauxStep::Hang]);
    let header = SessionHeader::new(cwd(), "m");
    drop(storage.create(&header).await.unwrap());

    let release = async {
        // Both calls are under way before the one setup goes on.
        tokio::task::yield_now().await;
        gate.notify_one();
    };
    let (first, second, ()) = tokio::join!(agent.open_session(&header.id), agent.open_session(&header.id), release);

    let (first, second) = (first.unwrap(), second.unwrap());
    assert_eq!(setup.calls.load(Ordering::SeqCst), 1);
    let run = first.send("go").unwrap();
    assert_eq!(second.send("too").err(), Some(SessionError::Busy));
    drop(run);
}

#[tokio::test]
async fn an_opening_that_fails_leaves_the_session_to_be_opened_later() {
    let (agent, storage, _) = agent([]);
    let header = SessionHeader::new(cwd(), "m");

    let error = agent.open_session(&header.id).await.err().unwrap();

    assert!(format!("{error:#}").contains("no session"), "{error:#}");
    assert!(agent.session(&header.id).is_none());
    drop(storage.create(&header).await.unwrap());
    agent.open_session(&header.id).await.unwrap();
}

#[tokio::test]
async fn an_adopted_transcript_is_a_session_of_the_agent_under_its_own_id() {
    let (agent, storage, provider) = agent([FauxStep::text("done")]);
    let mut transcript = Transcript::in_memory(SessionHeader::new(cwd(), "m"));
    transcript.append_message(Message::user("earlier")).await.unwrap();
    let id = transcript.header().id.clone();

    let session = agent.adopt_session(transcript, SessionParts::default()).unwrap();
    session.send("go").unwrap().wait().await;

    assert_eq!(provider.requests()[0].messages.len(), 2);
    assert!(agent.session(&id).is_some());
    // The host made the transcript: the agent's storage does not keep it.
    assert!(kept(&storage).await.is_empty());
    let again = Transcript::in_memory(SessionHeader {
        id: id.clone(),
        ..SessionHeader::new(cwd(), "m")
    });
    let error = agent.adopt_session(again, SessionParts::default()).err().unwrap();
    assert_eq!(error.to_string(), format!("session {id} is already open"));
}

#[test]
fn an_agent_no_session_could_run_with_is_refused() {
    let provider = Arc::new(FauxProvider::new([]));
    let twice = AgentConfig {
        tools: vec![Arc::new(Named("echo")), Arc::new(Named("echo"))],
        ..AgentConfig::new(provider.clone(), "m")
    };
    let unworkable = AgentConfig {
        compaction: compaction::Config {
            keep_recent_bytes: 0,
            ..compaction::Config::default()
        },
        ..AgentConfig::new(provider, "m")
    };

    let error = Agent::new(twice).err().unwrap();
    assert_eq!(error.to_string(), "more than one tool is named \"echo\"");
    assert!(Agent::new(unworkable).is_err());
}
