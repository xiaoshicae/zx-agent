//! What a test gives the code under test in place of the real thing: a provider that says
//! when a request starts, a tool that echoes and counts its calls, and the context a hook gets
//! when a test calls it directly.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use zx_agent::ai::{EventReceiver, Provider, Request, ToolDef};
use zx_agent::hooks::HookContext;
use zx_agent::input::{Input, InputQueues};
use zx_agent::tools::{ExecutionMode, Tool, ToolContext, ToolOutput};

/// A provider that says when a request has started, for tests that need the run to be under
/// way without waiting a fixed time. It hands each request to the provider it wraps, such as
/// a scripted one, and records it.
pub struct NotifyingProvider {
    provider: Box<dyn Provider>,
    requests: Mutex<Vec<Request>>,
    /// Notified at each request, with a permit kept for a waiter that comes later.
    pub started: Notify,
    /// The cancellation token of the latest request.
    pub token: Mutex<Option<CancellationToken>>,
}

impl NotifyingProvider {
    /// Wait until a request starts; panic if `running`, which makes the request, ends first.
    /// A test that needs a run under way waits for this rather than a fixed time.
    pub async fn wait_for_start(&self, running: impl Future) {
        tokio::select! {
            () = self.started.notified() => {}
            _ = running => panic!("the run ended before its request started"),
        }
    }

    /// `provider`, saying when each of its requests starts.
    pub fn new(provider: impl Provider + 'static) -> Arc<Self> {
        Arc::new(Self {
            provider: Box::new(provider),
            requests: Mutex::new(Vec::new()),
            started: Notify::new(),
            token: Mutex::new(None),
        })
    }

    /// Every request received so far, in order.
    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

impl Provider for NotifyingProvider {
    fn name(&self) -> &str {
        "notifying"
    }

    fn stream(&self, request: Request, cancel: CancellationToken) -> EventReceiver {
        *self.token.lock().unwrap() = Some(cancel.clone());
        self.requests.lock().unwrap().push(request.clone());
        let events = self.provider.stream(request, cancel);
        self.started.notify_one();
        events
    }
}

/// Echoes `text` after sleeping `ms`. Can inject queued messages while it runs (`steer`,
/// `follow_up`) or withdraw a queued user message (`withdraw`), report each string in
/// `progress` as a progress update, and return `details` and `usage` as given. `panic`
/// makes it panic, `fail` fail with that text, and `terminate` ask the run to end after it.
pub struct EchoTool {
    /// The name the model calls it by.
    pub name: &'static str,
    /// How its calls run beside other calls: in parallel unless a test says otherwise.
    pub mode: ExecutionMode,
    /// How many calls it has executed.
    pub calls: AtomicUsize,
    /// The queues of the session the tool runs in, once a test has told it (`feeds`).
    queues: OnceLock<Arc<InputQueues>>,
}

impl EchoTool {
    /// The tool called `name`, whose calls run in parallel.
    pub fn new(name: &'static str) -> Self {
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
    pub fn feeds(&self, queues: &Arc<InputQueues>) {
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

/// A hook context for calling a hook directly: a fresh run of a session called `session`, in
/// the current directory.
pub fn hook_context() -> HookContext {
    HookContext::new("session".into(), PathBuf::from("."), CancellationToken::new())
}
