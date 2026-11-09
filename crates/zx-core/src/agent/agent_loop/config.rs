//! What a loop runs with: the static specification, and the configuration that adds the
//! host's extensions to it.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::{
    ActiveTools, DEFAULT_APPROVAL_TIMEOUT, HookChain, Interaction, MessageQueues, ModelMeta, NoHuman, Provider,
    Reasoning, RunTrace, SlashCommand, SystemPrompt, Tool,
};

/// Static model and tool configuration, independent of per-run state and extensions.
pub struct LoopSpec {
    pub provider: Arc<dyn Provider>,
    pub model: String,
    /// Changes are recorded as system messages before the next request.
    pub system_prompt: SystemPrompt,
    pub tools: Vec<Arc<dyn Tool>>,
    pub cwd: PathBuf,
    pub max_tokens: Option<u32>,
    /// Sent with every task request; `None` leaves the server's default. Summary requests
    /// send none.
    pub reasoning: Option<Reasoning>,
}

impl LoopSpec {
    /// Check what the loop relies on: the model calls tools by the names they declare, so
    /// each tool must declare the name it is called by and no two may share one. A tool's
    /// `name`, `definition`, `execution_mode` and `effects` must not panic; they are tried
    /// here, so that a panic fails the run before its first request instead of escaping it.
    /// An attempt checks before its first request.
    pub fn validate(&self) -> Result<(), String> {
        self.validators().map(|_| ())
    }

    pub(super) fn validators(&self) -> Result<HashMap<String, jsonschema::Validator>, String> {
        let mut validators = HashMap::new();
        for (index, tool) in self.tools.iter().enumerate() {
            let described = std::panic::catch_unwind(AssertUnwindSafe(|| {
                (
                    tool.name().to_string(),
                    tool.definition(),
                    tool.execution_mode(),
                    tool.effects(),
                )
            }));
            let Ok((name, definition, _, _)) = described else {
                return Err(format!("tool {index} panicked while describing itself"));
            };
            if definition.name != name {
                return Err(format!(
                    "tool {name:?} declares itself as {:?}; the model calls a tool by the name it declares",
                    definition.name
                ));
            }
            if validators.contains_key(&name) {
                return Err(format!("more than one tool is named {name:?}"));
            }
            let validator = jsonschema::validator_for(&definition.parameters)
                .map_err(|error| format!("tool {name:?} has an invalid parameter schema: {error}"))?;
            validators.insert(name, validator);
        }
        Ok(validators)
    }
}

/// Host extensions and the static specification. Queues and the active tool set have shared
/// ownership because frontends and hooks change them while a run borrows this configuration.
pub struct LoopConfig {
    pub spec: LoopSpec,
    pub hooks: HookChain,
    pub interaction: Arc<dyn Interaction>,
    pub interaction_timeout: Duration,
    pub queues: Arc<MessageQueues>,
    /// Which of `spec.tools` the requests offer; every one by default.
    pub active_tools: Arc<ActiveTools>,
    /// Slash commands extensions registered, for a frontend to run.
    pub commands: Vec<Arc<dyn SlashCommand>>,
    /// What is known about models, by ID: the prices give each response a cost, the context
    /// window can set the token trigger of automatic summaries. Empty by default: costs and
    /// windows stay unknown.
    pub models: HashMap<String, ModelMeta>,
    /// Sent with every task request as its cache key ([`crate::ai::Request::cache_key`]), such as the
    /// session ID; `None` sends none.
    pub cache_key: Option<String>,
}

impl LoopConfig {
    pub fn new(spec: LoopSpec) -> Self {
        Self {
            spec,
            hooks: HookChain::new(),
            interaction: Arc::new(NoHuman),
            interaction_timeout: DEFAULT_APPROVAL_TIMEOUT,
            queues: Arc::new(MessageQueues::default()),
            active_tools: Arc::new(ActiveTools::default()),
            commands: Vec::new(),
            models: HashMap::new(),
            cache_key: None,
        }
    }

    /// The trace of a new run with a fresh ID, cancelled by `cancel`, whose hooks reach this
    /// configuration's queues and tool set.
    pub(crate) fn trace(&self, session_id: Option<String>, cancel: CancellationToken) -> RunTrace {
        RunTrace::new(
            session_id,
            self.spec.cwd.clone(),
            cancel,
            self.queues.clone(),
            self.active_tools.clone(),
        )
    }
}
