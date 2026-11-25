//! What a loop runs with: the model, the tools, and the host's hooks and policies.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::ai::{ModelMeta, Provider, Reasoning, SystemPrompt};
use crate::compaction::Summarizer;
use crate::guard::CancelSignal;
use crate::hooks::{HookChain, HookContext};
use crate::input::InputQueues;
use crate::policy::{ApprovalMode, PolicyChain};
use crate::runner::RunTrace;
use crate::tools::{ActiveTools, Toolbox};
use crate::transcript::Transcript;

/// What the loops of one session run with: the model and tools, and the host's hooks and
/// policies around them, as the agent assembled them for the session. Queues and the active
/// tool set have shared ownership because frontends and hooks change them while a run borrows
/// this configuration.
pub(crate) struct LoopConfig {
    pub(crate) provider: Arc<dyn Provider>,
    pub(crate) model: String,
    /// Changes are recorded as system messages before the next request.
    pub(crate) system_prompt: SystemPrompt,
    pub(crate) tools: Toolbox,
    /// The working directory of tools and hooks.
    pub(crate) cwd: PathBuf,
    pub(crate) max_tokens: Option<u32>,
    /// Sent with every task request; `None` leaves the server's default. Summary requests
    /// send none.
    pub(crate) reasoning: Option<Reasoning>,
    pub(crate) hooks: HookChain,
    /// The policies that decide whether a tool call may run; none allows every call.
    pub(crate) policies: PolicyChain,
    /// Who answers when a policy asks about a call.
    pub(crate) approvals: ApprovalMode,
    pub(crate) queues: Arc<InputQueues>,
    /// Which of `tools` the requests offer; every one by default.
    pub(crate) active_tools: Arc<ActiveTools>,
    /// What is known about models, by ID: the prices give each response a cost, and the
    /// window of `model` is what a token trigger that follows the window lies below.
    pub(crate) models: HashMap<String, ModelMeta>,
    /// Sent with every task request as its cache key ([`crate::ai::Request::cache_key`]);
    /// `None` sends none.
    pub(crate) cache_key: Option<String>,
    /// Makes the summaries of compactions; without one the kernel asks the model.
    pub(crate) summarizer: Option<Arc<dyn Summarizer>>,
}

impl LoopConfig {
    /// A loop that asks `model` of `provider`, with no system prompt, no tool, no hook and no
    /// human to approve calls, in the current directory.
    pub(crate) fn new(provider: Arc<dyn Provider>, model: impl Into<String>) -> Self {
        Self {
            provider,
            model: model.into(),
            system_prompt: SystemPrompt::default(),
            tools: Toolbox::default(),
            cwd: PathBuf::from("."),
            max_tokens: None,
            reasoning: None,
            hooks: HookChain::new(),
            policies: PolicyChain::new(),
            approvals: ApprovalMode::Deny,
            queues: Arc::new(InputQueues::default()),
            active_tools: Arc::new(ActiveTools::default()),
            models: HashMap::new(),
            cache_key: None,
            summarizer: None,
        }
    }

    /// The trace of the run `run_id` of `transcript`, cancelled by `cancel`: its hooks and
    /// policies are told the session's labels and who sent its latest input, and its hooks
    /// reach this configuration's queues and tool set.
    pub(crate) fn trace(&self, transcript: &Transcript, run_id: String, cancel: CancellationToken) -> RunTrace {
        let header = transcript.header();
        RunTrace::new(HookContext {
            session_id: header.id.clone(),
            run_id,
            cwd: self.cwd.clone(),
            labels: header.labels.clone(),
            actor: transcript.latest_actor().map(String::from),
            cancel: CancelSignal::new(cancel),
            queues: Arc::clone(&self.queues),
            active_tools: Arc::clone(&self.active_tools),
        })
    }
}
