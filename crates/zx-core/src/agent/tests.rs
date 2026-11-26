use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::ai::faux::{FauxProvider, FauxStep};
use crate::ai::{EventReceiver, Message, ModelCost, Request, ToolDef, event_channel};
use crate::events::RunOutcome;
use crate::hooks::{Hook, HookContext, HookResult};
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
async fn a_relative_working_directory_is_recorded_as_the_absolute_one() {
    let (agent, storage, _) = agent([]);

    let session = agent.create_session(SessionOptions::new(".")).await.unwrap();

    // Taken from the process's current directory when the session is created, so that the
    // session works there whenever it is opened again.
    let current = std::env::current_dir().unwrap();
    assert_eq!(session.cwd(), current);
    let list = storage.list(&SessionQuery::default()).await.unwrap();
    assert_eq!(list.sessions[0].cwd, current.display().to_string());
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

/// Remembers the sessions whose input it saw.
struct Remember(Arc<Mutex<Vec<String>>>);

#[async_trait]
impl Hook for Remember {
    async fn on_input(&self, text: String, ctx: &HookContext) -> HookResult<String> {
        lock(&self.0).push(ctx.session_id.clone());
        Ok(text)
    }
}

#[tokio::test]
async fn the_hooks_of_the_agent_serve_every_session() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("one"), FauxStep::text("two")]));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut config = AgentConfig::new(provider, "m");
    config.hooks.push("remember", Arc::new(Remember(seen.clone())));
    let agent = Agent::new(config).unwrap();

    let mut sessions = Vec::new();
    for _ in 0..2 {
        let session = agent.create_session(SessionOptions::new(cwd())).await.unwrap();
        session.send("go").unwrap().wait().await;
        sessions.push(session.id().to_string());
    }

    assert_eq!(*lock(&seen), sessions);
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

/// A provider called `name`, whose streams end at once.
struct Called(&'static str);

impl Provider for Called {
    fn name(&self) -> &str {
        self.0
    }

    fn stream(&self, _request: Request, _cancel: CancellationToken) -> EventReceiver {
        let (_sender, receiver) = event_channel();
        receiver
    }
}

/// An agent of the model `m` with a cost budget of `budget`.
fn budgeted(budget: f64) -> AgentConfig {
    AgentConfig {
        limits: RunLimits {
            cost_budget: Some(budget),
            ..RunLimits::default()
        },
        ..AgentConfig::new(Arc::new(FauxProvider::new([])), "m")
    }
}

/// An agent of the model `m`, about which `meta` is known.
fn knowing(meta: ModelMeta) -> AgentConfig {
    AgentConfig {
        models: [("m".to_string(), meta)].into(),
        ..AgentConfig::new(Arc::new(FauxProvider::new([])), "m")
    }
}

/// An agent of the model `m` at the prices `cost`.
fn priced(cost: ModelCost) -> AgentConfig {
    knowing(ModelMeta {
        cost: Some(cost),
        ..ModelMeta::default()
    })
}

/// Agents of the model `m` that no session could run with, one for each rule but those of the
/// model's and the provider's names, each with words of its refusal.
fn unusable() -> Vec<(AgentConfig, &'static str)> {
    let config = || AgentConfig::new(Arc::new(FauxProvider::new([])), "m");
    let free = ModelCost::default();
    vec![
        (
            AgentConfig {
                tools: vec![Arc::new(Named("echo")), Arc::new(Named("echo"))],
                ..config()
            },
            "more than one tool is named \"echo\"",
        ),
        (
            AgentConfig {
                compaction: compaction::Config {
                    keep_recent_bytes: 0,
                    ..compaction::Config::default()
                },
                ..config()
            },
            "keep_recent_bytes must be greater than zero",
        ),
        (budgeted(f64::NAN), "the cost budget must be"),
        (budgeted(f64::INFINITY), "the cost budget must be"),
        (budgeted(-1.0), "the cost budget must be"),
        (
            priced(ModelCost {
                input: f64::NAN,
                ..free
            }),
            "model \"m\": prices",
        ),
        (priced(ModelCost { output: -1.0, ..free }), "model \"m\": prices"),
        (
            priced(ModelCost {
                cache_read: f64::INFINITY,
                ..free
            }),
            "model \"m\": prices",
        ),
        (
            priced(ModelCost {
                cache_write: -0.5,
                ..free
            }),
            "model \"m\": prices",
        ),
        (
            knowing(ModelMeta {
                context_window: Some(0),
                ..ModelMeta::default()
            }),
            "model \"m\": the context window must be greater than zero",
        ),
        (
            AgentConfig {
                approvals: ApprovalMode::Wait(Duration::ZERO),
                ..config()
            },
            "the wait for an approval must be longer than zero",
        ),
        (
            AgentConfig {
                max_tokens: Some(0),
                ..config()
            },
            "max_tokens must be greater than zero",
        ),
    ]
}

#[test]
fn an_agent_no_session_could_run_with_is_refused() {
    let mut wrong = Vec::new();
    for (config, expected) in unusable() {
        match Agent::new(config) {
            Err(error) if error.to_string().contains(expected) => {}
            Err(error) => wrong.push(format!("refused with {error:?} instead of {expected:?}")),
            Ok(_) => wrong.push(format!("accepted what {expected:?} refuses")),
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
    let provider = Arc::new(FauxProvider::new([]));
    for blank in ["", " \t"] {
        let error = Agent::new(AgentConfig::new(provider.clone(), blank)).err().unwrap();
        assert_eq!(error.downcast_ref(), Some(&SessionError::InvalidModel), "{blank:?}");
        let error = Agent::new(AgentConfig::new(Arc::new(Called(blank)), "m")).err();
        assert_eq!(
            error.as_ref().and_then(|error| error.downcast_ref()),
            Some(&SessionError::InvalidProvider),
            "{blank:?}"
        );
    }
    // Zero is a budget like any other, and a price too.
    assert!(Agent::new(budgeted(0.0)).is_ok());
    assert!(Agent::new(priced(ModelCost::default())).is_ok());
}
