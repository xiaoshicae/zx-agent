//! Driving a session: [`RunnerSetup`], a builder with test defaults, gives a [`Driven`],
//! which derefs to the `Session`; its `run`, `prompt`, `continue_run` and `compact` read a run
//! to its end, collecting its events in a [`Collect`] and answering its approval requests with
//! an [`Approver`], and `session()` copies the record. A test of `Run` itself calls
//! `Session::send`, and cancels with the session's own `cancel()`.

use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use zx_agent::agent::RunLimits;
use zx_agent::ai::retry::RetryPolicy;
use zx_agent::ai::{Message, Provider};
use zx_agent::compaction;
use zx_agent::events::{Event, EventKind, RunReport};
use zx_agent::input::{Input, PendingInput};
use zx_agent::policy::{ApprovalDecision, ApprovalMode};
use zx_agent::session::SessionError;
use zx_agent::tools::Tool;
use zx_agent::transcript::{LoadedSession, Transcript};
use zx_agent::{Agent, AgentConfig, Run, Session};

use crate::sessions::{Seed, Start, header};

/// The role of a test's agent: the model `test-model` of `provider`, with `tools`. It has no
/// system prompt, so its runs add no system message; the tests of the system prompt set
/// their own.
pub fn config(provider: Arc<dyn Provider>, tools: Vec<Arc<dyn Tool>>) -> AgentConfig {
    AgentConfig {
        tools,
        ..AgentConfig::new(provider, "test-model")
    }
}

/// The bounds the runner tests use unless they set their own: 30 model requests and 15
/// minutes, those of the unattended agent (the kernel's own defaults are unlimited).
pub fn test_limits() -> RunLimits {
    RunLimits {
        max_model_requests: Some(30),
        timeout: Some(Duration::from_secs(900)),
        token_budget: None,
        cost_budget: None,
    }
}

/// Retries with millisecond delays, so retry tests stay fast.
pub fn fast_retry(max_retries: u32) -> RetryPolicy {
    RetryPolicy {
        max_retries,
        base_delay: Duration::from_millis(10),
        max_delay: Duration::from_millis(50),
        ..RetryPolicy::default()
    }
}

/// What a test's session is made of, with test defaults: no retries, unattended preset
/// limits, a new session in memory, no compaction settings, nobody to answer approvals.
/// Override fields with struct update syntax, then `build`. An agent made from the setup
/// creates or opens the session, as a host's agent does; the session works in the directory
/// its header names.
pub struct RunnerSetup {
    /// The agent's role. Its `retry`, `limits` and `compaction` are not used: the fields
    /// below say them, so that a test sets each in one place. Its `storage` keeps a new
    /// session; a session the test wrote is opened where the test wrote it.
    pub config: AgentConfig,
    /// How a failed response is tried again; it is not, by default.
    pub retry: RetryPolicy,
    /// The bounds of each run: [`test_limits`] by default.
    pub limits: RunLimits,
    /// When and how the session's context is summarized; never on its own by default.
    pub compaction: compaction::Config,
    /// The session the runs go to: a new one by default.
    pub session: Start,
    /// Who answers the approval requests of the runs. With one, the session waits for
    /// answers, two minutes unless `config.approvals` says how long.
    pub approver: Option<Arc<dyn Approver>>,
}

impl RunnerSetup {
    /// The test defaults, around the role `config`.
    pub fn new(config: AgentConfig) -> Self {
        Self {
            config,
            retry: fast_retry(0),
            limits: test_limits(),
            compaction: compaction::Config::default(),
            session: Start::default(),
            approver: None,
        }
    }

    /// The session, and the events of its runs. Panics for settings that no session can be
    /// made from; [`refusal`] gives the reason instead.
    pub async fn build(self) -> (Driven, Arc<Collect>) {
        self.watched(None).await
    }

    /// The session and the events of its runs, with `watch` called for each event as it is
    /// read, with the session, which it may cancel.
    pub async fn build_with(self, watch: impl Fn(&Event, &Session) + Send + Sync + 'static) -> (Driven, Arc<Collect>) {
        self.watched(Some(Arc::new(watch))).await
    }

    async fn watched(self, watch: Option<Watch>) -> (Driven, Arc<Collect>) {
        let Self {
            mut config,
            retry,
            limits,
            compaction,
            session,
            approver,
        } = self;
        if approver.is_some() && config.approvals == ApprovalMode::Deny {
            config.approvals = ApprovalMode::Wait(APPROVAL_TIMEOUT);
        }
        config.retry = retry;
        config.limits = limits;
        config.compaction = compaction;
        let session = session_of(config, session)
            .await
            .unwrap_or_else(|error| panic!("the test's session cannot be made: {error:#}"));
        let events = Arc::new(Collect::default());
        let driven = Driven {
            session,
            events: events.clone(),
            watch,
            approver,
        };
        (driven, events)
    }
}

/// The session `start` names, which an agent with the role `config` creates or opens. The
/// agent goes once the session is made: a session needs it no longer.
async fn session_of(mut config: AgentConfig, start: Start) -> anyhow::Result<Session> {
    match start {
        Start::New(options) => Agent::new(config)?.create_session(options).await,
        Start::Open { storage, id } => {
            config.storage = storage;
            Agent::new(config)?.open_session(&id).await
        }
    }
}

/// Why no agent can be made from `config`.
pub fn refusal(config: AgentConfig) -> String {
    match Agent::new(config) {
        Ok(_) => panic!("an agent was made from the configuration"),
        Err(error) => format!("{error:#}"),
    }
}

/// A call that waits for approval, as the run announced it.
pub struct Asked {
    /// The ID of the call.
    pub tool_call_id: String,
    /// The name of the tool it calls.
    pub tool: String,
    /// The arguments the call would run with.
    pub args: Value,
    /// Why the policy asks.
    pub reason: String,
}

/// Answers the approval requests of a test's runs, as a person at a frontend would: while
/// it decides, the run is read on, so the run's timeout and cancellation still take effect.
#[async_trait]
pub trait Approver: Send + Sync {
    /// The answer to `request`; `None` leaves it unanswered.
    async fn confirm(&self, request: &Asked) -> Option<ApprovalDecision>;
}

/// How long a test's runs wait for its [`Approver`]: long enough that it always answers
/// first, so that a test that expects no answer leaves the approver out instead.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);

/// What a test does with each event of a run as it is read; it gets the session too.
type Watch = Arc<dyn Fn(&Event, &Session) + Send + Sync>;

/// A session whose runs a test starts with one call that reads them to their end,
/// collecting their events and answering their approval requests. Everything else is the
/// [`Session`]'s own, which it derefs to: a test cancels a run with `cancel()`, and starts
/// one it reads itself with `send`. A handle, like the session: its clones are the same.
#[derive(Clone)]
pub struct Driven {
    session: Session,
    events: Arc<Collect>,
    watch: Option<Watch>,
    approver: Option<Arc<dyn Approver>>,
}

/// An approval request on its way to an answer: the request's ID, and the answer.
type Answering = Pin<Box<dyn Future<Output = (String, Option<ApprovalDecision>)> + Send>>;

impl Deref for Driven {
    type Target = Session;

    fn deref(&self) -> &Session {
        &self.session
    }
}

impl Driven {
    /// Run `message` to the end. Panics when the session does not take the run, as while
    /// another one runs; [`Self::prompt`] returns that refusal instead.
    pub async fn run(&self, message: impl Into<Input>) -> RunReport {
        self.prompt(message).await.expect("the session takes the run")
    }

    /// Run `message` to the end. Busy when the session already runs.
    pub async fn prompt(&self, message: impl Into<Input>) -> Result<RunReport, SessionError> {
        Ok(self.read(self.session.send(message)?).await)
    }

    /// Continue the conversation without new input, to the end.
    pub async fn continue_run(&self) -> Result<RunReport, SessionError> {
        Ok(self.read(self.session.continue_run()?).await)
    }

    /// Summarize the old prefix of the conversation.
    pub async fn compact(&self) -> Result<compaction::Report, SessionError> {
        Ok(self.read(self.session.compact()?).await)
    }

    /// A copy of the session's transcript as it is now, to read.
    pub fn session(&self) -> Transcript {
        self.session
            .with_transcript(copy_of)
            .expect("the transcript is read between runs")
    }

    /// Read `run` to its end, collecting its events and answering its approval requests.
    async fn read<T>(&self, mut run: Run<T>) -> T {
        // The approver's answer under way; the run asks for one approval at a time.
        let mut answering: Option<Answering> = None;
        loop {
            tokio::select! {
                biased;
                (approval_id, decision) = answered(&mut answering) => {
                    answering = None;
                    if let Some(decision) = decision {
                        // The run may have stopped waiting; then the answer changes nothing.
                        drop(self.session.resolve_approval(&approval_id, decision));
                    }
                }
                event = run.next() => match event {
                    Some(event) => {
                        self.events.emit(&event);
                        if let Some(watch) = &self.watch {
                            watch(&event, &self.session);
                        }
                        if let Some(asked) = self.ask(&event) {
                            answering = Some(asked);
                        }
                    }
                    None => break,
                },
            }
        }
        run.wait().await
    }

    /// The approver's answer to the request that `event` announces, if it announces one
    /// and somebody answers.
    fn ask(&self, event: &Event) -> Option<Answering> {
        let EventKind::ApprovalRequested {
            approval_id,
            tool_call_id,
            tool_name,
            args,
            reason,
        } = &event.event
        else {
            return None;
        };
        let approver = self.approver.clone()?;
        let approval_id = approval_id.clone();
        let asked = Asked {
            tool_call_id: tool_call_id.clone(),
            tool: tool_name.clone(),
            args: args.clone(),
            reason: reason.clone(),
        };
        Some(Box::pin(async move { (approval_id, approver.confirm(&asked).await) }))
    }
}

/// The answer under way, once it is there; never without one.
async fn answered(answering: &mut Option<Answering>) -> (String, Option<ApprovalDecision>) {
    match answering {
        Some(answer) => answer.await,
        None => std::future::pending().await,
    }
}

/// A read-only copy of `transcript`: its records, and its position in the tree.
fn copy_of(transcript: &Transcript) -> Transcript {
    let loaded = LoadedSession {
        header: transcript.header().clone(),
        records: transcript
            .entries()
            .iter()
            .map(|entry| serde_json::to_string(entry).unwrap())
            .collect(),
        incomplete_tail_bytes: transcript.incomplete_tail_bytes(),
    };
    let mut copy = Transcript::snapshot(loaded).expect("a session's own records load");
    match transcript.leaf_id() {
        Some(leaf) => copy.branch(leaf).expect("the leaf is one of the records"),
        None => copy.reset_leaf(),
    }
    copy
}

/// The input `session` had queued and no run consumed, taken without the holds.
pub fn take_queued_inputs(session: &Session) -> Result<Vec<Input>, SessionError> {
    session
        .take_queued()
        .map(|queued| queued.into_iter().map(PendingInput::into_input).collect())
}

/// The events of the runs a test drove, in the order they were read.
#[derive(Default)]
pub struct Collect(Mutex<Vec<Event>>);

impl Collect {
    fn emit(&self, event: &Event) {
        self.0.lock().unwrap().push(event.clone());
    }

    /// The events with their envelopes.
    pub fn envelopes(&self) -> Vec<Event> {
        self.0.lock().unwrap().clone()
    }

    /// The events without their envelopes.
    pub fn events(&self) -> Vec<EventKind> {
        self.envelopes().into_iter().map(|event| event.event).collect()
    }

    /// The kind of each event, such as `step_start`.
    pub fn kinds(&self) -> Vec<&'static str> {
        self.events().iter().map(EventKind::kind).collect()
    }

    /// Task steps started: the `step_start` events.
    pub fn steps(&self) -> usize {
        self.kinds().iter().filter(|kind| **kind == "step_start").count()
    }
}

/// Run one user `prompt` after the messages of `context` with a 10-request budget and no
/// retries, the defaults most loop tests need. The run goes over a session in memory that
/// starts with `context`; afterwards `context` is that session's projection. Returns the
/// report and the events.
pub async fn run_prompt(config: AgentConfig, context: &mut Vec<Message>, prompt: &str) -> (RunReport, Arc<Collect>) {
    run_prompt_with(config, context, prompt, 10).await
}

/// `run_prompt` with at most `max_model_requests`.
pub async fn run_prompt_with(
    config: AgentConfig,
    context: &mut Vec<Message>,
    prompt: &str,
    max_model_requests: u32,
) -> (RunReport, Arc<Collect>) {
    let mut session = Seed::in_memory(header()).await;
    for message in context.iter() {
        session.append_message(message.clone()).await.unwrap();
    }
    let limits = RunLimits {
        max_model_requests: Some(max_model_requests),
        ..RunLimits::default()
    };
    let (runner, sink) = RunnerSetup {
        limits,
        session: session.into(),
        ..RunnerSetup::new(config)
    }
    .build()
    .await;
    let report = runner.run(prompt).await;
    *context = runner.session().messages();
    (report, sink)
}

/// Run one user `prompt` on a new session in memory whose approval requests `approver`
/// answers. Returns the report and the events.
pub async fn run_approved(config: AgentConfig, approver: Arc<dyn Approver>, prompt: &str) -> (RunReport, Arc<Collect>) {
    let (runner, events) = RunnerSetup {
        approver: Some(approver),
        ..RunnerSetup::new(config)
    }
    .build()
    .await;
    let report = runner.run(prompt).await;
    (report, events)
}
