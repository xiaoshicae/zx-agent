//! Helpers shared by the integration tests.

#![allow(
    dead_code,
    reason = "each test binary uses a different subset of these helpers; `expect` would fail in a binary that uses them all"
)]

pub(crate) mod fake_feishu;
pub(crate) mod fake_mcp;
pub(crate) mod fake_slack;
pub(crate) mod http;
pub(crate) mod im;
pub(crate) mod mock_server;
pub(crate) mod summary_eval;

use std::future::Future;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use tokio::sync::Notify;
use zx_agent::agent::SessionParts;
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::retry::RetryPolicy;
use zx_agent::ai::{
    AssistantMessage, ContentBlock, EventReceiver, Message, Provider, Request, StopReason, ToolCall, ToolDef, Usage,
};
use zx_agent::compaction;
use zx_agent::events::{Event, EventKind};
use zx_agent::hooks::HookContext;
use zx_agent::input::{Input, InputQueues};
use zx_agent::policy::{ApprovalDecision, ApprovalMode, DEFAULT_APPROVAL_TIMEOUT};
use zx_agent::runner::{RunLimits, RunReport};
use zx_agent::session::SessionError;
use zx_agent::tools::{ExecutionMode, Tool, ToolContext, ToolOutput};
use zx_agent::transcript::{Entry, JsonlStorage, LoadedSession, SessionHeader, SessionStorage, Transcript};
use zx_agent::{Agent, AgentConfig, Run, Session};

/// What a finished program wrote to stdout, decoded lossily.
pub(crate) fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// What a finished program wrote to stderr, decoded lossily.
pub(crate) fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Send `name` (as `kill` spells it, such as `TERM`) to `child`.
#[cfg(unix)]
pub(crate) async fn signal(child: &tokio::process::Child, name: &str) {
    assert!(
        tokio::process::Command::new("kill")
            .args([name, &child.id().unwrap().to_string()])
            .status()
            .await
            .unwrap()
            .success()
    );
}

/// An answer by `provider` and `model` that asked for `tool` with `arguments` and was cut
/// off before the call ran: the shape a crash leaves in a session file.
pub(crate) fn tool_call_answer(provider: &str, model: &str, tool: &str, arguments: Value) -> Message {
    let mut answer = AssistantMessage::failure(provider, model, StopReason::ToolUse, "");
    answer.error_message = None;
    answer.content = vec![ContentBlock::ToolCall(ToolCall {
        id: "c1".into(),
        name: tool.into(),
        arguments,
        arguments_error: None,
    })];
    Message::Assistant(answer)
}

/// A finished answer that reported `input` prompt tokens and 10 output tokens, as the last
/// answer of a long conversation does.
pub(crate) fn answer_reporting(input: u64) -> Message {
    let mut answer = AssistantMessage::failure("p", "m", StopReason::Stop, "");
    answer.error_message = None;
    answer.content = vec![ContentBlock::Text { text: "noted".into() }];
    answer.usage = Usage {
        input,
        output: 10,
        total_tokens: input + 10,
        ..Usage::default()
    };
    Message::Assistant(answer)
}

/// A session with one old message worth summarizing and a recent input; returns the old
/// message's entry ID.
pub(crate) async fn seeded_session() -> (Transcript, String) {
    let mut store = Transcript::in_memory(header());
    let old = store
        .append_message(Message::user("observed ".repeat(600)))
        .await
        .unwrap();
    store.append_message(Message::user("most recent input")).await.unwrap();
    (store, old)
}

/// A summary citing `id`, as the JSON the model answers a summary request with.
pub(crate) fn summary_json(id: &str) -> String {
    json!({"facts":[{"text":"Old observation","sourceEntryIds":[id]}],"inferences":["A guess"],"plans":["Continue"]})
        .to_string()
}

/// Every request of the session, rebuilt from its checkpoints, in order.
pub(crate) fn replayed_requests(store: &Transcript) -> Vec<Request> {
    store
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            Entry::RequestStarted(started) => Some(store.replay_request(&started.id).unwrap().1),
            _ => None,
        })
        .collect()
}

/// A scripted provider that says when a request has started, for tests that need the run to
/// be under way without waiting a fixed time.
pub(crate) struct NotifyingProvider {
    pub(crate) faux: FauxProvider,
    /// Notified at each request, with a permit kept for a waiter that comes later.
    pub(crate) started: Notify,
    /// The cancellation token of the latest request.
    pub(crate) token: Mutex<Option<CancellationToken>>,
}

impl NotifyingProvider {
    pub(crate) fn new(steps: impl IntoIterator<Item = FauxStep>) -> Arc<Self> {
        Arc::new(Self {
            faux: FauxProvider::new(steps),
            started: Notify::new(),
            token: Mutex::new(None),
        })
    }

    /// Every request received so far, in order.
    pub(crate) fn requests(&self) -> Vec<Request> {
        self.faux.requests()
    }
}

impl Provider for NotifyingProvider {
    fn name(&self) -> &str {
        "notifying"
    }

    fn stream(&self, request: Request, cancel: CancellationToken) -> EventReceiver {
        *self.token.lock().unwrap() = Some(cancel.clone());
        let events = self.faux.stream(request, cancel);
        self.started.notify_one();
        events
    }
}

/// The bounds the runner tests use unless they set their own: 30 model requests and 15
/// minutes, those of the unattended agent (the kernel's own defaults are unlimited).
pub(crate) fn test_limits() -> RunLimits {
    RunLimits {
        max_model_requests: Some(30),
        timeout: Some(Duration::from_secs(900)),
        token_budget: None,
        cost_budget: None,
    }
}

/// Echoes `text` after sleeping `ms`. Can inject queued messages while it runs (`steer`,
/// `follow_up`) or withdraw a queued user message (`withdraw`), report each string in
/// `progress` as a progress update, and return `details` and `usage` as given.
pub(crate) struct EchoTool {
    pub name: &'static str,
    pub mode: ExecutionMode,
    pub calls: AtomicUsize,
    /// The queues of the session the tool runs in, once a test has told it (`feeds`).
    queues: OnceLock<Arc<InputQueues>>,
}

impl EchoTool {
    pub(crate) fn new(name: &'static str) -> Self {
        Self {
            name,
            mode: ExecutionMode::Parallel,
            calls: AtomicUsize::new(0),
            queues: OnceLock::new(),
        }
    }

    /// Let the calls that ask for it (`steer`, `follow_up`, `withdraw`) change `queues`,
    /// those of the session the tool runs in. A session makes its own queues, so a test
    /// says this once the session exists.
    pub(crate) fn feeds(&self, queues: &Arc<InputQueues>) {
        assert!(self.queues.set(queues.clone()).is_ok(), "a tool feeds one session");
    }
}

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        self.name
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: self.name.to_string(),
            description: "Echo text".to_string(),
            parameters: json!({"type": "object"}),
        }
    }

    fn execution_mode(&self) -> ExecutionMode {
        self.mode
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        for update in args.get("progress").and_then(Value::as_array).into_iter().flatten() {
            assert!(ctx.report_progress(update.as_str().unwrap_or_default()));
        }
        if let Some(ms) = args.get("ms").and_then(Value::as_u64) {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
        if let (Some(queues), Some(text)) = (self.queues.get(), args.get("steer").and_then(Value::as_str)) {
            queues.steer(text);
        }
        if let (Some(queues), Some(text)) = (self.queues.get(), args.get("follow_up").and_then(Value::as_str)) {
            queues.follow_up(text);
        }
        if let (Some(queues), Some(text)) = (self.queues.get(), args.get("withdraw").and_then(Value::as_str)) {
            assert!(queues.withdraw(&Input::from(text)), "{text:?} is not queued");
        }
        if args.get("panic").is_some() {
            panic!("tool bug");
        }
        if let Some(error) = args.get("fail").and_then(Value::as_str) {
            return Err(error.to_string());
        }
        let usage = match args.get("usage") {
            Some(usage) => Some(serde_json::from_value(usage.clone()).map_err(|error| error.to_string())?),
            None => None,
        };
        Ok(ToolOutput {
            content: args.get("text").and_then(Value::as_str).unwrap_or_default().to_string(),
            is_error: false,
            terminate: args.get("terminate").and_then(Value::as_bool).unwrap_or(false),
            details: args.get("details").cloned(),
            usage,
        })
    }
}

/// The events of the runs a test drove, in the order they were read.
#[derive(Default)]
pub(crate) struct Collect(pub Mutex<Vec<Event>>);

impl Collect {
    fn emit(&self, event: &Event) {
        self.0.lock().unwrap().push(event.clone());
    }

    /// The events with their envelopes.
    pub(crate) fn envelopes(&self) -> Vec<Event> {
        self.0.lock().unwrap().clone()
    }

    /// The events without their envelopes.
    pub(crate) fn events(&self) -> Vec<EventKind> {
        self.envelopes().into_iter().map(|event| event.event).collect()
    }

    pub(crate) fn kinds(&self) -> Vec<&'static str> {
        self.events().iter().map(EventKind::kind).collect()
    }

    /// Task steps started: the `step_start` events.
    pub(crate) fn steps(&self) -> usize {
        self.kinds().iter().filter(|kind| **kind == "step_start").count()
    }

    /// Tool call ids of `tool_execution_end` events, in emission order.
    pub(crate) fn tool_end_order(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                EventKind::ToolExecutionEnd { tool_call_id, .. } => Some(tool_call_id),
                _ => None,
            })
            .collect()
    }
}

pub(crate) fn config(provider: Arc<dyn Provider>, tools: Vec<Arc<dyn Tool>>) -> AgentConfig {
    // No system prompt, so runs add no system message; system prompt tests set their own.
    AgentConfig {
        tools,
        ..AgentConfig::new(provider, "test-model")
    }
}

/// Why no agent can be made from `config`.
pub(crate) fn refusal(config: AgentConfig) -> String {
    match Agent::new(config) {
        Ok(_) => panic!("an agent was made from the configuration"),
        Err(error) => format!("{error:#}"),
    }
}

/// Short text of each message, for compact assertions.
pub(crate) fn summarize(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .map(|message| match message {
            Message::System(system) => {
                let names: Vec<&str> = system.sections.iter().map(|change| change.name.as_str()).collect();
                format!("system:{}", names.join(","))
            }
            Message::User { content, .. } => format!("user:{content}"),
            Message::Assistant(assistant) => {
                let calls: Vec<String> = assistant.tool_calls().into_iter().map(|c| c.id).collect();
                if calls.is_empty() {
                    format!("assistant:{}", assistant.text())
                } else {
                    format!("assistant:calls[{}]", calls.join(","))
                }
            }
            Message::ToolResult(result) => {
                format!(
                    "tool:{}={}{}",
                    result.tool_call_id,
                    result.content,
                    if result.is_error { "!" } else { "" }
                )
            }
        })
        .collect()
}

/// Run one user `prompt` after the messages of `context` with a 10-request budget, no
/// retries and no cancellation, the defaults most loop tests need. The run goes over an
/// in-memory session that starts with `context`; afterwards `context` is that session's
/// projection. Returns the report and the events.
pub(crate) async fn run_prompt(
    config: AgentConfig,
    context: &mut Vec<Message>,
    prompt: &str,
) -> (RunReport, Arc<Collect>) {
    run_prompt_with(config, context, prompt, 10, &CancellationToken::new()).await
}

/// `run_prompt` with at most `max_model_requests`, stopped by `cancel`.
pub(crate) async fn run_prompt_with(
    config: AgentConfig,
    context: &mut Vec<Message>,
    prompt: &str,
    max_model_requests: u32,
    cancel: &CancellationToken,
) -> (RunReport, Arc<Collect>) {
    let mut session = Transcript::in_memory(header());
    for message in context.iter() {
        session.append_message(message.clone()).await.unwrap();
    }
    let limits = RunLimits {
        max_model_requests: Some(max_model_requests),
        ..RunLimits::default()
    };
    let (mut runner, sink) = RunnerSetup {
        limits,
        session,
        ..RunnerSetup::new(config)
    }
    .build();
    let report = runner.run(prompt, cancel).await;
    *context = runner.session().messages();
    (report, sink)
}

/// Run one user `prompt` on a new in-memory session whose approval requests `approver`
/// answers. Returns the report and the events.
pub(crate) async fn run_approved(
    config: AgentConfig,
    approver: Arc<dyn Approver>,
    prompt: &str,
) -> (RunReport, Arc<Collect>) {
    let (mut runner, events) = RunnerSetup {
        approver: Some(approver),
        ..RunnerSetup::new(config)
    }
    .build();
    let report = runner.run(prompt, &CancellationToken::new()).await;
    (report, events)
}

/// A hook context for calling a hook directly: a fresh run of a session called `session`, in
/// the current directory.
pub(crate) fn hook_context() -> HookContext {
    HookContext::new("session".into(), PathBuf::from("."), CancellationToken::new())
}

/// Retries with millisecond delays, so retry tests stay fast.
pub(crate) fn fast_retry(max_retries: u32) -> RetryPolicy {
    RetryPolicy {
        max_retries,
        base_delay: Duration::from_millis(10),
        max_delay: Duration::from_millis(50),
        ..RetryPolicy::default()
    }
}

/// The header of a new session that works in the current directory.
pub(crate) fn header() -> SessionHeader {
    header_in(Path::new("."))
}

/// The header of a new session that works in `cwd`.
pub(crate) fn header_in(cwd: &Path) -> SessionHeader {
    SessionHeader::new(cwd, "test-model")
}

/// Every line of `text` parsed as JSON, as the JSON-lines outputs are read.
pub(crate) fn json_lines(text: &str) -> Vec<Value> {
    text.lines().map(|line| serde_json::from_str(line).unwrap()).collect()
}

/// Every line of a session file, parsed.
pub(crate) fn read_lines(path: &Path) -> Vec<Value> {
    json_lines(&std::fs::read_to_string(path).unwrap())
}

/// A new session with `header` in the file `path`, through the storage of that one file.
pub(crate) async fn create_session_file(path: &Path, header: SessionHeader) -> anyhow::Result<Transcript> {
    let log = JsonlStorage::file(path).create(&header).await?;
    Ok(Transcript::with_log(header, log))
}

/// The session in the file `path`, loaded and open for appending.
pub(crate) async fn open_session_file(path: &Path) -> anyhow::Result<Transcript> {
    let id = read_session_file(path)?.header().id.clone();
    let (loaded, log) = JsonlStorage::file(path).open(&id).await?;
    Transcript::load(loaded, log)
}

/// A snapshot of the session in the file `path`: its header and complete lines, read here
/// and now, so that a callback, which cannot wait for a storage, can read it too.
pub(crate) fn read_session_file(path: &Path) -> anyhow::Result<Transcript> {
    let text = std::fs::read_to_string(path)?;
    let mut lines = text.lines();
    let header = serde_json::from_str(lines.next().unwrap_or_default())?;
    Transcript::snapshot(LoadedSession {
        header,
        records: lines.map(str::to_string).collect(),
        incomplete_tail_bytes: 0,
    })
}

/// A call that waits for approval, as the run announced it.
pub(crate) struct Asked {
    pub tool_call_id: String,
    pub tool: String,
    /// The arguments the call would run with.
    pub args: Value,
    /// Why the policy asks.
    pub reason: String,
}

/// Answers the approval requests of a test's runs, as a person at a frontend would: while
/// it decides, the run is read on, so the run's timeout and cancellation still take effect.
#[async_trait]
pub(crate) trait Approver: Send + Sync {
    /// The answer to `request`; `None` leaves it unanswered.
    async fn confirm(&self, request: &Asked) -> Option<ApprovalDecision>;
}

/// What a test's session is made of, with test defaults: no retries, unattended preset
/// limits, an in-memory transcript, no compaction settings, nobody to answer approvals.
/// Override fields with struct update syntax, then `build`. The session works in the
/// directory its transcript's header names.
pub(crate) struct RunnerSetup {
    /// The agent's role. Its `retry`, `limits` and `compaction` are not used: the fields
    /// below and `Runner::with_compaction` say them, so that a test sets each in one place.
    pub config: AgentConfig,
    pub retry: RetryPolicy,
    pub limits: RunLimits,
    pub session: Transcript,
    /// Who answers the approval requests of the runs. With one, the session waits for
    /// answers, two minutes unless `config.approvals` says how long.
    pub approver: Option<Arc<dyn Approver>>,
}

impl RunnerSetup {
    pub(crate) fn new(config: AgentConfig) -> Self {
        Self {
            config,
            retry: fast_retry(0),
            limits: test_limits(),
            session: Transcript::in_memory(header()),
            approver: None,
        }
    }

    /// The runner and the events of its runs.
    pub(crate) fn build(self) -> (Runner, Arc<Collect>) {
        let events = Arc::new(Collect::default());
        let runner = Runner {
            parts: Mutex::new(Some(Parts {
                setup: self,
                compaction: compaction::Config::default(),
                watch: None,
            })),
            driven: OnceLock::new(),
            events: events.clone(),
        };
        (runner, events)
    }

    /// The runner and the events of its runs, with `watch` called for each event as it is
    /// read.
    pub(crate) fn build_with(self, watch: impl Fn(&Event) + Send + Sync + 'static) -> (Runner, Arc<Collect>) {
        let (runner, events) = self.build();
        if let Some(parts) = runner.parts.lock().unwrap().as_mut() {
            parts.watch = Some(Arc::new(watch));
        }
        (runner, events)
    }
}

/// What a test does with each event of a run as it is read.
type Watch = Arc<dyn Fn(&Event) + Send + Sync>;

/// What a session is made of, until it is made.
struct Parts {
    setup: RunnerSetup,
    compaction: compaction::Config,
    watch: Option<Watch>,
}

/// A session driven the way the tests of runs want it: one run at a time, read to its end,
/// with its events collected. The session is made when it is first needed, so that a test
/// can still give it its compaction settings.
pub(crate) struct Runner {
    parts: Mutex<Option<Parts>>,
    driven: OnceLock<Driven>,
    events: Arc<Collect>,
}

impl Runner {
    /// The session, made when it is first asked for.
    fn driven(&self) -> &Driven {
        self.driven.get_or_init(|| {
            let parts = self.parts.lock().unwrap().take().expect("the session is made once");
            let RunnerSetup {
                mut config,
                retry,
                limits,
                session,
                approver,
            } = parts.setup;
            if approver.is_some() && config.approvals == ApprovalMode::Deny {
                config.approvals = ApprovalMode::Wait(DEFAULT_APPROVAL_TIMEOUT);
            }
            config.retry = retry;
            config.limits = limits;
            config.compaction = parts.compaction;
            // The agent goes once the session is made: a session needs it no longer.
            let session = Agent::new(config)
                .and_then(|agent| agent.adopt_session(session, SessionParts::default()))
                .unwrap_or_else(|error| panic!("the test's session cannot be made: {error:#}"));
            Driven {
                session,
                events: self.events.clone(),
                watch: parts.watch,
                approver,
            }
        })
    }

    /// Opt into automatic context preparation or tune manual summary budgets, before the
    /// first run. Rejects budgets that cannot work.
    pub(crate) fn with_compaction(self, config: compaction::Config) -> anyhow::Result<Self> {
        config.validate()?;
        self.parts
            .lock()
            .unwrap()
            .as_mut()
            .expect("compaction is set before the first run")
            .compaction = config;
        Ok(self)
    }

    /// Run `message` to the end; cancelling `cancel` cancels the run.
    pub(crate) async fn run(&mut self, message: impl Into<Input>, cancel: &CancellationToken) -> RunReport {
        let run = self.driven().prompt(message, cancel);
        run.await.expect("one run at a time")
    }

    /// Continue the conversation without new input, to the end.
    pub(crate) async fn continue_run(&mut self, cancel: &CancellationToken) -> RunReport {
        let run = self.driven().continue_run(cancel);
        run.await.expect("one run at a time")
    }

    /// Summarize the old prefix of the conversation.
    pub(crate) async fn compact(&mut self, cancel: &CancellationToken) -> compaction::Report {
        let run = self.driven().compact(cancel);
        run.await.expect("one run at a time")
    }

    /// A copy of the session's transcript as it is now, to read.
    pub(crate) fn session(&self) -> Transcript {
        self.driven()
            .with_transcript(copy_of)
            .expect("the transcript is read between runs")
    }

    /// The session's queues.
    pub(crate) fn queues(&self) -> &Arc<InputQueues> {
        self.driven().queues()
    }

    /// The session itself, for a test that shares it or calls what only a session has; its
    /// runs still go to the same events.
    pub(crate) fn into_session(self) -> Driven {
        self.driven();
        self.driven.into_inner().expect("made above")
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

/// A session whose runs a test starts with one call that reads them to their end,
/// collecting their events. Everything else is the [`Session`]'s own, which it derefs to.
#[derive(Clone)]
pub(crate) struct Driven {
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
    /// Run `message` to the end; cancelling `cancel` cancels the run. Busy when the session
    /// already runs.
    pub(crate) async fn prompt(
        &self,
        message: impl Into<Input>,
        cancel: &CancellationToken,
    ) -> Result<RunReport, SessionError> {
        Ok(self.read(self.session.send(message)?, cancel).await)
    }

    /// Continue the conversation without new input, to the end.
    pub(crate) async fn continue_run(&self, cancel: &CancellationToken) -> Result<RunReport, SessionError> {
        Ok(self.read(self.session.continue_run()?, cancel).await)
    }

    /// Summarize the old prefix of the conversation.
    pub(crate) async fn compact(&self, cancel: &CancellationToken) -> Result<compaction::Report, SessionError> {
        Ok(self.read(self.session.compact()?, cancel).await)
    }

    /// Read `run` to its end, collecting its events and answering its approval requests;
    /// `cancel`, once cancelled, cancels the run, which is still read to its end.
    async fn read<T>(&self, mut run: Run<T>, cancel: &CancellationToken) -> T {
        let mut cancelled = false;
        // The approver's answer under way; the run asks for one approval at a time.
        let mut answering: Option<Answering> = None;
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled(), if !cancelled => {
                    self.session.cancel();
                    cancelled = true;
                }
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
                            watch(&event);
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
