//! Helpers shared by the integration tests.

#![allow(
    dead_code,
    reason = "each test binary uses a different subset of these helpers; `expect` would fail in a binary that uses them all"
)]

pub mod mock_server;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use zx_agent::agent::{
    AgentEvent, EventSink, ExecutionMode, LoopConfig, LoopControl, LoopResult, LoopSpec, MessageQueues, Tool,
    ToolContext, ToolOutput, ToolPolicy, run_loop,
};
use zx_agent::ai::retry::RetryPolicy;
use zx_agent::ai::{Message, Provider, SystemPrompt, ToolDef};
use zx_agent::interaction::ApprovalCache;
use zx_agent::policy::PolicyHook;
use zx_agent::presets::troubleshoot;
use zx_agent::runner::{Limits, Runner};
use zx_agent::session::{SessionHeader, SessionStore};

/// Echoes `text` after sleeping `ms`. Can inject queued messages while it runs.
pub struct EchoTool {
    pub name: &'static str,
    pub mode: ExecutionMode,
    pub calls: AtomicUsize,
    pub queues: Option<Arc<MessageQueues>>,
}

impl EchoTool {
    pub fn new(name: &'static str) -> Self {
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

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(ms) = args.get("ms").and_then(Value::as_u64) {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
        if let (Some(queues), Some(text)) = (&self.queues, args.get("steer").and_then(Value::as_str)) {
            queues.steer(text);
        }
        if let (Some(queues), Some(text)) = (&self.queues, args.get("follow_up").and_then(Value::as_str)) {
            queues.follow_up(text);
        }
        if args.get("panic").is_some() {
            panic!("tool bug");
        }
        if let Some(error) = args.get("fail").and_then(Value::as_str) {
            return Err(error.to_string());
        }
        Ok(ToolOutput {
            content: args.get("text").and_then(Value::as_str).unwrap_or_default().to_string(),
            is_error: false,
            terminate: args.get("terminate").and_then(Value::as_bool).unwrap_or(false),
        })
    }
}

#[derive(Default)]
pub struct Collect(pub Mutex<Vec<AgentEvent>>);

impl EventSink for Collect {
    fn emit(&self, event: AgentEvent) {
        self.0.lock().unwrap().push(event);
    }
}

impl Collect {
    pub fn events(&self) -> Vec<AgentEvent> {
        self.0.lock().unwrap().clone()
    }

    pub fn kinds(&self) -> Vec<&'static str> {
        self.events().iter().map(AgentEvent::kind).collect()
    }

    /// Tool call ids of `tool_execution_end` events, in emission order.
    pub fn tool_end_order(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                AgentEvent::ToolExecutionEnd { tool_call_id, .. } => Some(tool_call_id),
                _ => None,
            })
            .collect()
    }
}

pub fn config(provider: Arc<dyn Provider>, tools: Vec<Arc<dyn Tool>>) -> LoopConfig {
    LoopConfig::new(LoopSpec {
        provider,
        model: "test-model".to_string(),
        // Empty, so runs add no system message; system prompt tests set their own.
        system_prompt: SystemPrompt::default(),
        tools,
        cwd: PathBuf::from("."),
        max_tokens: None,
        sequential_tools: false,
    })
}

pub fn with_policy(mut config: LoopConfig, policy: Arc<dyn ToolPolicy>) -> LoopConfig {
    config.hooks.push(PolicyHook::new(policy));
    config
}

/// Short text of each message, for compact assertions.
pub fn summarize(messages: &[Message]) -> Vec<String> {
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

/// Run one user `prompt` on `context` with a 10-turn budget and no cancellation, the
/// defaults most loop tests need. Returns the result and the events.
pub async fn run_prompt(config: &LoopConfig, context: &mut Vec<Message>, prompt: &str) -> (LoopResult, Collect) {
    let sink = Collect::default();
    let prompts = vec![Message::user(prompt)];
    let mut approvals = ApprovalCache::default();
    let cancel = CancellationToken::new();
    let mut control = LoopControl::new(Some(10), &mut approvals, &cancel, &sink);
    let result = run_loop(context, prompts, config, &mut control).await;
    (result, sink)
}

/// Retries with millisecond delays, so retry tests stay fast.
pub fn fast_retry(max_retries: u32) -> RetryPolicy {
    RetryPolicy {
        max_retries,
        base_delay: Duration::from_millis(10),
        max_delay: Duration::from_millis(50),
    }
}

pub fn header() -> SessionHeader {
    SessionHeader::new(Path::new("."), "test-model")
}

/// Every line of a session file, parsed.
pub fn read_lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// The parts of a runner, with test defaults: no retries, unattended preset limits,
/// an in-memory session. Override fields with struct update syntax, then `build`.
pub struct RunnerSetup {
    pub config: LoopConfig,
    pub retry: RetryPolicy,
    pub limits: Limits,
    pub session: SessionStore,
}

impl RunnerSetup {
    pub fn new(config: LoopConfig) -> Self {
        Self {
            config,
            retry: fast_retry(0),
            limits: troubleshoot::limits(),
            session: SessionStore::in_memory(header()),
        }
    }

    /// The runner and the sink that records its events.
    pub fn build(self) -> (Runner, Arc<Collect>) {
        let sink = Arc::new(Collect::default());
        (self.build_with(sink.clone()), sink)
    }

    /// The runner, sending events to `sink`.
    pub fn build_with(self, sink: Arc<dyn EventSink>) -> Runner {
        Runner::new(self.config, self.retry, self.limits, self.session, sink)
    }
}

/// Cancels a token at the first streamed delta, to cancel a response mid-stream without
/// depending on timing.
pub struct CancelOnFirstDelta(pub CancellationToken);

impl EventSink for CancelOnFirstDelta {
    fn emit(&self, event: AgentEvent) {
        if matches!(event, AgentEvent::MessageUpdate { .. }) {
            self.0.cancel();
        }
    }
}
