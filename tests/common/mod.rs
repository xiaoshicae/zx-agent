//! Helpers shared by the integration tests.

#![allow(
    dead_code,
    reason = "each test binary uses a different subset of these helpers; `expect` would fail in a binary that uses them all"
)]

pub(crate) mod evals;
pub(crate) mod fake_feishu;
pub(crate) mod fake_mcp;
pub(crate) mod fake_slack;
pub(crate) mod http;
pub(crate) mod im;
pub(crate) mod mock_server;
pub(crate) mod summary_eval;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use tokio::sync::Notify;
use zx_agent::agent::ApprovalCache;
use zx_agent::agent::QueuedMessage;
use zx_agent::agent::{
    AgentEvent, Attempt, AttemptResult, Event, EventSink, ExecutionMode, HookContext, LoopConfig, LoopSpec,
    MessageQueues, Tool, ToolContext, ToolOutput, ToolPolicy,
};
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::retry::RetryPolicy;
use zx_agent::ai::{
    AssistantMessage, ContentBlock, EventReceiver, Message, Provider, Request, StopReason, SystemPrompt, ToolDef,
};
use zx_agent::policy::PolicyHook;
use zx_agent::runner::{Limits, Runner};
use zx_agent::session::{Entry, SessionHeader, SessionStore};

/// The compiled `zx-agent`, for the tests that drive the program.
pub(crate) fn zx_agent() -> tokio::process::Command {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_zx-agent"))
}

/// What a finished program wrote to stdout, decoded lossily.
pub(crate) fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// What a finished program wrote to stderr, decoded lossily.
pub(crate) fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Every line of `text` parsed as JSON, as the JSON-lines outputs are read.
pub(crate) fn json_lines(text: &str) -> Vec<Value> {
    text.lines().map(|line| serde_json::from_str(line).unwrap()).collect()
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

/// Where chat and acp keep the sessions of `cwd` under `home` by default, spelled out as pi
/// spells it.
pub(crate) fn session_dir(home: &Path, cwd: &Path) -> PathBuf {
    let encoded = cwd.display().to_string().trim_start_matches('/').replace('/', "-");
    home.join(".zx-agent/sessions").join(format!("--{encoded}--"))
}

/// The session files in `dir`, by name; none when the directory does not exist.
pub(crate) fn session_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = read_dir
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "jsonl"))
        .collect();
    files.sort();
    files
}

/// An answer by `provider` and `model` that asked for `tool` with `arguments` and was cut
/// off before the call ran: the shape a crash leaves in a session file.
pub(crate) fn tool_call_answer(provider: &str, model: &str, tool: &str, arguments: Value) -> Message {
    let mut answer = AssistantMessage::failure(provider, model, StopReason::ToolUse, "");
    answer.error_message = None;
    answer.content = vec![ContentBlock::ToolCall {
        id: "c1".into(),
        name: tool.into(),
        arguments,
        arguments_error: None,
    }];
    Message::Assistant(answer)
}

/// A session with one old message worth summarizing and a recent input; returns the old
/// message's entry ID.
pub(crate) async fn seeded_session() -> (SessionStore, String) {
    let mut store = SessionStore::in_memory(header());
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
pub(crate) fn replayed_requests(store: &SessionStore) -> Vec<Request> {
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
/// minutes, those of the unattended profiles (the kernel's own defaults are unlimited).
pub(crate) fn test_limits() -> Limits {
    Limits {
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
    pub queues: Option<Arc<MessageQueues>>,
}

impl EchoTool {
    pub(crate) fn new(name: &'static str) -> Self {
        Self {
            name,
            mode: ExecutionMode::Parallel,
            calls: AtomicUsize::new(0),
            queues: None,
        }
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
        if let (Some(queues), Some(text)) = (&self.queues, args.get("steer").and_then(Value::as_str)) {
            queues.steer(text);
        }
        if let (Some(queues), Some(text)) = (&self.queues, args.get("follow_up").and_then(Value::as_str)) {
            queues.follow_up(text);
        }
        if let (Some(queues), Some(text)) = (&self.queues, args.get("withdraw").and_then(Value::as_str)) {
            assert!(
                queues.withdraw(&QueuedMessage::User(text.into())),
                "{text:?} is not queued"
            );
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

#[derive(Default)]
pub(crate) struct Collect(pub Mutex<Vec<Event>>);

impl EventSink for Collect {
    fn emit(&self, event: &Event) {
        self.0.lock().unwrap().push(event.clone());
    }
}

impl Collect {
    /// The events with their envelopes.
    pub(crate) fn envelopes(&self) -> Vec<Event> {
        self.0.lock().unwrap().clone()
    }

    /// The events without their envelopes.
    pub(crate) fn events(&self) -> Vec<AgentEvent> {
        self.envelopes().into_iter().map(|event| event.event).collect()
    }

    pub(crate) fn kinds(&self) -> Vec<&'static str> {
        self.events().iter().map(AgentEvent::kind).collect()
    }

    /// Tool call ids of `tool_execution_end` events, in emission order.
    pub(crate) fn tool_end_order(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                AgentEvent::ToolExecutionEnd { tool_call_id, .. } => Some(tool_call_id),
                _ => None,
            })
            .collect()
    }
}

pub(crate) fn config(provider: Arc<dyn Provider>, tools: Vec<Arc<dyn Tool>>) -> LoopConfig {
    LoopConfig::new(LoopSpec {
        provider,
        model: "test-model".to_string(),
        // Empty, so runs add no system message; system prompt tests set their own.
        system_prompt: SystemPrompt::default(),
        tools,
        cwd: PathBuf::from("."),
        max_tokens: None,
        reasoning: None,
        sequential_tools: false,
    })
}

pub(crate) fn with_policy(mut config: LoopConfig, policy: Arc<dyn ToolPolicy>) -> LoopConfig {
    config.hooks.push(PolicyHook::new(policy));
    config
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

/// Run one user `prompt` after the messages of `context` with a 10-request budget and no
/// cancellation, the defaults most loop tests need. The attempt runs over an in-memory
/// session that starts with `context`; afterwards `context` is that session's projection.
/// Returns the result and the events.
pub(crate) async fn run_prompt(
    config: &LoopConfig,
    context: &mut Vec<Message>,
    prompt: &str,
) -> (AttemptResult, Collect) {
    let mut session = SessionStore::in_memory(header());
    for message in context.iter() {
        session.append_message(message.clone()).await.unwrap();
    }
    let sink = Collect::default();
    let mut approvals = ApprovalCache::default();
    let cancel = CancellationToken::new();
    let mut attempt = Attempt::new(config, &mut session, &mut approvals, &cancel, &sink);
    attempt.max_model_requests = Some(10);
    let result = attempt.run(vec![QueuedMessage::User(prompt.to_string())]).await;
    *context = session.messages();
    (result, sink)
}

/// Run one attempt with `prompts` over a new in-memory session, with at most
/// `max_model_requests`, stopped by `cancel` and reporting to `sink`. Returns the result and
/// the session's projection.
pub(crate) async fn run_attempt(
    config: &LoopConfig,
    prompts: Vec<QueuedMessage>,
    max_model_requests: Option<u32>,
    cancel: &CancellationToken,
    sink: &dyn EventSink,
) -> (AttemptResult, Vec<Message>) {
    let mut session = SessionStore::in_memory(header());
    let mut approvals = ApprovalCache::default();
    let mut attempt = Attempt::new(config, &mut session, &mut approvals, cancel, sink);
    attempt.max_model_requests = max_model_requests;
    let result = attempt.run(prompts).await;
    (result, session.messages())
}

/// A hook context for calling a hook directly: a fresh run in the current directory.
pub(crate) fn hook_context() -> HookContext {
    HookContext::new(None, PathBuf::from("."), CancellationToken::new())
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

pub(crate) fn header() -> SessionHeader {
    SessionHeader::new(Path::new("."), "test-model")
}

/// Every line of a session file, parsed.
pub(crate) fn read_lines(path: &Path) -> Vec<Value> {
    json_lines(&std::fs::read_to_string(path).unwrap())
}

/// The parts of a runner, with test defaults: no retries, unattended preset limits,
/// an in-memory session. Override fields with struct update syntax, then `build`.
pub(crate) struct RunnerSetup {
    pub config: LoopConfig,
    pub retry: RetryPolicy,
    pub limits: Limits,
    pub session: SessionStore,
}

impl RunnerSetup {
    pub(crate) fn new(config: LoopConfig) -> Self {
        Self {
            config,
            retry: fast_retry(0),
            limits: test_limits(),
            session: SessionStore::in_memory(header()),
        }
    }

    /// The runner and the sink that records its events.
    pub(crate) fn build(self) -> (Runner, Arc<Collect>) {
        let sink = Arc::new(Collect::default());
        (self.build_with(sink.clone()), sink)
    }

    /// The runner, sending events to `sink`.
    pub(crate) fn build_with(self, sink: Arc<dyn EventSink>) -> Runner {
        Runner::new(self.config, self.retry, self.limits, self.session, sink)
    }
}

/// Cancels a token at the first streamed delta, to cancel a response mid-stream without
/// depending on timing.
pub(crate) struct CancelOnFirstDelta(pub CancellationToken);

impl EventSink for CancelOnFirstDelta {
    fn emit(&self, event: &Event) {
        if matches!(event.event, AgentEvent::MessageUpdate { .. }) {
            self.0.cancel();
        }
    }
}

/// A pseudo-terminal: the master end for the test, and the slave end to give a child as
/// its terminal (stdin or stdout).
#[cfg(unix)]
pub(crate) fn pty() -> (tokio::fs::File, std::os::fd::OwnedFd) {
    use std::os::fd::{FromRawFd, OwnedFd};
    let mut master = -1;
    let mut slave = -1;
    // SAFETY: both output pointers refer to initialized local integers, and null optional
    // name/termios/winsize pointers request the OS defaults. Success yields two fresh FDs.
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
    // SAFETY: openpty succeeded; each descriptor has exactly one owner from here on.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    (tokio::fs::File::from_std(std::fs::File::from(master)), slave)
}
