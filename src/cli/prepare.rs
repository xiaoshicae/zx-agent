//! The assembly of one session, shared by run, chat, im and acp: the agent with the
//! command line over it, in the environment of the settings, the provider, the tools, the
//! session file and the hooks every mode registers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use zx_agent::AgentSession;
use zx_agent::agent::{EventSink, HookOptions, Hooks, LoopConfig, PolicyDecision, Tool};
use zx_agent::ai::ModelMeta;
use zx_agent::ai::openai::{OpenAiCompatible, OpenAiConfig};
use zx_agent::ai::retry::RetryPolicy;
use zx_agent::compaction;
use zx_agent::config::Execution;
use zx_agent::config::skills::{ReadSkillTool, TOOL_NAME};
use zx_agent::config::{AgentDefinition, Builtin};
use zx_agent::continuation::ContinueOnLength;
use zx_agent::executor::Executor;
use zx_agent::extensions;
use zx_agent::otel;
use zx_agent::output_store::{self, OutputStore, ReadOutputTool, SpillOutputs};
use zx_agent::policy::{AutoApprove, CommandPolicy, PolicyHook, Rule, RuleMatcher, RulePolicy};
use zx_agent::prompt::{self, build_system_prompt};
use zx_agent::redact::RedactSecrets;
use zx_agent::runner::{Limits, Runner};
use zx_agent::session::{SessionHeader, SessionStore};
use zx_agent::tools::{self, BASH};

use super::execution::{check_session_outside, executor};
use super::{BaseArgs, BudgetArgs, Settings, Shared, env_value, resolve_cwd};
use crate::session_files::{self, SessionFile};

/// Register a built-in hook under a fixed name, which `hook_failed` events and `[hook]` lines
/// show.
///
/// Without one the name is the hook's type name, whose form Rust does not guarantee across
/// compiler versions, and the policy hooks would all share a name. MCP server names cannot
/// contain `_`, so these never clash with an extension's.
fn push_built_in(config: &mut LoopConfig, name: &str, hook: impl Hooks + 'static) {
    config.hooks.push_with(
        hook,
        HookOptions {
            name: Some(name.to_string()),
            ..HookOptions::default()
        },
    );
}

/// What a mode gives [`prepare`]: the command line, the model, the session file and what the
/// process shares.
pub(crate) struct SessionOptions<'a> {
    pub(crate) base: &'a BaseArgs,
    pub(crate) budget: &'a BudgetArgs,
    pub(crate) model: &'a str,
    /// The working directory, over `--cwd`: a client's project directory. `None` keeps the
    /// command line's.
    pub(crate) cwd: Option<&'a Path>,
    pub(crate) file: SessionFile<'a>,
    /// The built-in agent the mode runs, or the user's file that replaces it.
    pub(crate) agent: Builtin,
    pub(crate) shared: &'a Shared,
}

/// A configured session with the hooks every mode shares.
///
/// A mode adds only what is its own, such as chat's approval rule and interaction host, then
/// calls `into_session`.
pub(crate) struct PreparedSession {
    pub(crate) config: LoopConfig,
    retry: RetryPolicy,
    pub(crate) store: SessionStore,
    /// The agent's auto-approved commands, for frontend policies that also ask for bash.
    auto_approve: AutoApprove,
    /// The agent's `permissions.approveAll`: nothing waits for a person.
    approve_all: bool,
    limits: Limits,
    compaction: compaction::Config,
    /// The selected agent's name, for a frontend to show.
    pub(crate) agent_name: String,
    /// The agent's auto-approved command prefixes, for a frontend to show.
    pub(crate) auto_approve_commands: Vec<String>,
}

impl PreparedSession {
    /// Ask a person before every bash command, except those the agent auto-approves, or
    /// none when the agent approves every call; other tools follow the agent's permissions.
    /// Chat and im both work this way.
    pub(crate) fn require_bash_approval(&mut self, reason: &str) {
        if self.approve_all {
            return;
        }
        let policy = RulePolicy::new(vec![
            Rule {
                matcher: RuleMatcher::Tool(BASH.into()),
                decision: PolicyDecision::Ask(reason.into()),
            },
            Rule {
                matcher: RuleMatcher::Any,
                decision: PolicyDecision::Allow,
            },
        ])
        .auto_approve(std::mem::take(&mut self.auto_approve));
        push_built_in(&mut self.config, "bash_approval", PolicyHook::new(Arc::new(policy)));
    }

    /// The conversation, sending events to `sink`: the one place where every mode turns a
    /// prepared session into a running one.
    pub(crate) fn into_session(self, sink: Arc<dyn EventSink>) -> Result<AgentSession> {
        let runner =
            Runner::new(self.config, self.retry, self.limits, self.store, sink).with_compaction(self.compaction)?;
        Ok(AgentSession::from(runner))
    }
}

/// Refuse request fields that the reasoning level would also set: which one the server gets
/// would not be clear from the configuration.
fn check_reasoning_fields(settings: &Settings) -> Result<()> {
    if settings.reasoning.is_some()
        && let Some(field) = settings
            .reasoning_format
            .fields()
            .iter()
            .find(|field| settings.request_fields.contains_key(**field))
    {
        anyhow::bail!(
            "provider.requestFields.{field} conflicts with reasoning, which sets it with provider.reasoningFormat {}; use one of them",
            settings.reasoning_format.as_str()
        );
    }
    Ok(())
}

/// Assemble a session: startup reads, short session appends and, in container mode, starting
/// the container precede the run.
///
/// Configuration errors and a container that cannot start are reported before a new session
/// file is created.
pub(crate) async fn prepare(options: SessionOptions<'_>) -> Result<PreparedSession> {
    let (cwd, mut agent, models) = settle(&options)?;
    let context = agent.context_section(&cwd).context(
        "cannot load project instructions; an agent with discover = false under [context] skips AGENTS.md discovery",
    )?;
    // Before the session file: invalid provider settings are configuration errors too.
    let (provider, api_key) = provider(&options)?;
    let mut host = host_tools(&mut agent, &options.shared.settings.config.tool_output, &cwd)?;
    let (session, executor, environment) = open_session(&options, &cwd).await?;
    let tools = assemble_tools(&agent, &options, &executor, &mut host)?;
    // No allowlist: the commands an agent allows run without asking, the others ask.
    let policy = CommandPolicy::new(Vec::<String>::new()).deny_also(agent.permissions.denied_commands());
    let command_policy = command_policy_section(&policy, &tools);
    let system_prompt = build_system_prompt(
        &agent.instructions,
        &[
            (prompt::ENVIRONMENT, &environment),
            (prompt::COMMAND_POLICY, &command_policy),
            (prompt::PROJECT_CONTEXT, &context),
            (prompt::AVAILABLE_SKILLS, &host.catalog),
        ],
    );
    let mut config = LoopConfig {
        system_prompt,
        tools,
        cwd,
        max_tokens: options.shared.settings.max_tokens,
        reasoning: options.shared.settings.reasoning,
        ..LoopConfig::new(Arc::new(provider), options.model)
    };
    push_built_in(&mut config, "command_policy", PolicyHook::new(Arc::new(policy)));
    // Each session's redaction owns its list: the hook reads the files again before every use.
    let redaction = RedactSecrets::new(api_key.into_iter().chain(hidden_values(&options)))
        .with_secret_files(options.shared.secret_files.clone());
    let auto_approve = register_hooks(&mut config, &mut agent, &options, redaction, host.spill.take())?;
    config.models = models;
    config.cache_key = options
        .shared
        .settings
        .prompt_cache_key
        .then(|| session.header().id.clone());
    Ok(PreparedSession {
        config,
        retry: RetryPolicy {
            max_retries: options.shared.settings.max_retries,
            ..RetryPolicy::default()
        },
        store: session,
        auto_approve,
        approve_all: agent.permissions.approve_all,
        limits: agent.limits,
        compaction: agent.compaction,
        agent_name: agent.name,
        auto_approve_commands: agent.permissions.allowed_commands(),
    })
}

/// The working directory, the agent with the command line over it, and what is known about
/// the models. A cost budget needs the model's prices; container execution uses the
/// directory's real path.
fn settle(options: &SessionOptions<'_>) -> Result<(PathBuf, AgentDefinition, HashMap<String, ModelMeta>)> {
    let cwd = resolve_cwd(options.cwd.or(options.base.cwd.as_deref()))?;
    let mut agent = load_agent(options)?;
    let models = model_meta(&mut agent, &options.shared.settings.config.models, options.model);
    // Without prices every response costs nothing, and the budget would never stop a run.
    anyhow::ensure!(
        agent.limits.cost_budget.is_none() || models.get(options.model).is_some_and(|meta| meta.cost.is_some()),
        "a cost budget needs the prices of the model {:?}: add them to [[models]] in config.toml",
        options.model
    );
    // The container mounts the workspace at its real path, which tools must use as well.
    let cwd = match options.shared.settings.config.execution {
        Execution::Local => cwd,
        Execution::Container => cwd
            .canonicalize()
            .with_context(|| format!("cannot resolve {}", cwd.display()))?,
    };
    Ok((cwd, agent, models))
}

/// The built-in tools the agent enables, running on `executor`, and the tools of this
/// machine.
fn assemble_tools(
    agent: &AgentDefinition,
    options: &SessionOptions<'_>,
    executor: &Arc<dyn Executor>,
    host: &mut HostTools,
) -> Result<Vec<Arc<dyn Tool>>> {
    let built_in: Vec<String> = agent.tools.iter().filter(|name| !host_tool(name)).cloned().collect();
    let mut tools = tools::builtin_tools(&built_in, options.shared.settings.tool_timeout, executor)?;
    tools.extend(std::mem::take(&mut host.tools));
    Ok(tools)
}

/// The command policy's prompt section when bash is among `tools`: the policy only concerns
/// bash.
fn command_policy_section(policy: &CommandPolicy, tools: &[Arc<dyn Tool>]) -> String {
    if tools.iter().any(|tool| tool.name() == BASH) {
        policy.prompt_section().unwrap_or_default()
    } else {
        String::new()
    }
}

/// The selected agent with the command line's tools and budgets over it, validated: explicit
/// values replace the agent's.
fn load_agent(options: &SessionOptions<'_>) -> Result<AgentDefinition> {
    let config = &options.shared.settings.config;
    let mut agent = AgentDefinition::load(options.agent, options.shared.settings.home.as_deref(), config)?;
    if let Some(tools) = &options.base.tools {
        agent.tools = tools
            .iter()
            .map(|name| name.trim())
            .filter(|name| !name.is_empty())
            .map(String::from)
            .collect();
    }
    apply_budgets(&mut agent, options.budget);
    agent.validate(config)?;
    Ok(agent)
}

/// `--max-model-requests`, `--timeout-secs` and the token and cost budgets; a zero timeout or
/// budget removes the agent's.
fn apply_budgets(agent: &mut AgentDefinition, budget: &BudgetArgs) {
    if let Some(turns) = budget.max_model_requests {
        agent.limits.max_model_requests = Some(turns);
    }
    if let Some(secs) = budget.timeout_secs {
        agent.limits.timeout = (secs > 0).then(|| Duration::from_secs(secs));
    }
    if let Some(tokens) = budget.token_budget {
        agent.limits.token_budget = (tokens > 0).then_some(tokens);
    }
    if let Some(amount) = budget.cost_budget {
        agent.limits.cost_budget = (amount > 0.0).then_some(amount);
    }
}

/// What is known about the models: the entries of config.toml. With automatic summaries on, a
/// known window of `model` adds a token trigger below it.
fn model_meta(
    agent: &mut AgentDefinition,
    known: &HashMap<String, ModelMeta>,
    model: &str,
) -> HashMap<String, ModelMeta> {
    let models = known.clone();
    if agent.compaction.trigger_bytes.is_some()
        && let Some(window) = models.get(model).and_then(|meta| meta.context_window)
    {
        agent.compaction.derive_trigger_tokens(window);
    }
    models
}

/// The provider of the settings, and the API key it authenticates with.
fn provider(options: &SessionOptions<'_>) -> Result<(OpenAiCompatible, Option<String>)> {
    let settings = &options.shared.settings;
    // Local servers (vLLM, Ollama, llama.cpp) often need no key, so a missing key is allowed.
    let api_key = env_value(&settings.api_key_env);
    check_reasoning_fields(settings)?;
    let provider = OpenAiCompatible::new(OpenAiConfig {
        base_url: settings.base_url.clone(),
        // Provider authentication and output filtering each need an owned copy.
        api_key: api_key.clone().into(),
        idle_timeout: settings.idle_timeout,
        system_updates: settings.system_updates,
        extra_fields: settings.request_fields.clone(),
        fill_missing_reasoning: settings.fill_missing_reasoning,
        reasoning_format: settings.reasoning_format,
        headers: settings.headers.clone(),
        prompt_cache_key: settings.prompt_cache_key,
        ..OpenAiConfig::default()
    })
    .context("invalid [provider] settings in config.toml")?;
    Ok((provider, api_key))
}

/// Whether `name` is a tool of this machine rather than of the executor: read_skill and
/// read_output read files here, whatever executor runs commands.
fn host_tool(name: &str) -> bool {
    name == TOOL_NAME || name == output_store::TOOL_NAME
}

/// The tools of this machine (see [`host_tool`]) that the agent enables, with the skill
/// catalog for the prompt and the hook that saves long results.
struct HostTools {
    tools: Vec<Arc<dyn Tool>>,
    catalog: String,
    spill: Option<SpillOutputs>,
}

fn host_tools(agent: &mut AgentDefinition, tool_output: &output_store::Config, cwd: &Path) -> Result<HostTools> {
    let mut host = HostTools {
        tools: Vec::new(),
        catalog: String::new(),
        spill: None,
    };
    if agent.tools.iter().any(|name| name == TOOL_NAME) && !agent.skills.is_empty() {
        let tool = ReadSkillTool::new(std::mem::take(&mut agent.skills))?;
        host.catalog = tool.catalog();
        host.tools.push(Arc::new(tool));
    }
    if agent.tools.iter().any(|name| name == output_store::TOOL_NAME) {
        let store = OutputStore::new(cwd, tool_output.clone())?;
        // The immutable path/config is used independently by the reading tool and writer hook.
        host.spill = Some(SpillOutputs::new(
            store.clone(),
            vec![output_store::TOOL_NAME.into(), TOOL_NAME.into()],
        ));
        host.tools.push(Arc::new(ReadOutputTool::new(store)));
    }
    Ok(host)
}

/// The session, where commands run (on this machine without the variables of [`hidden_env`]),
/// and the prompt section describing that.
///
/// An existing session is opened first, for its ID; a new one is created only after the
/// container, if any, has started, so that a failed start leaves no session file behind.
async fn open_session(options: &SessionOptions<'_>, cwd: &Path) -> Result<(SessionStore, Arc<dyn Executor>, String)> {
    let config = &options.shared.settings.config;
    if config.execution == Execution::Container {
        check_session_outside(&options.file, cwd)?;
    }
    let header = SessionHeader::new(cwd, options.model);
    let opened = session_files::open_existing(&options.file)?;
    let session_id = opened
        .as_ref()
        .map_or_else(|| header.id.clone(), |session| session.header().id.clone());
    let (executor, environment) = executor(config, cwd, &session_id, hidden_env(options)).await?;
    let session = match opened {
        Some(session) => session,
        None => session_files::create(options.file, header)?,
    };
    if session.recovered_tail_bytes() > 0 {
        eprintln!(
            "[session] recovered incomplete final record; removed {} bytes",
            session.recovered_tail_bytes()
        );
    }
    Ok((session, executor, environment))
}

/// Register the hooks every mode shares, after the command policy: the MCP servers, the
/// agent's permissions, continuation, `redaction` and `spill`; returns the auto-approved commands
/// for the frontend's own bash policy.
///
/// Each hook declares its stage, so the policies run first and redaction runs before output
/// is saved, whatever is registered later.
fn register_hooks(
    config: &mut LoopConfig,
    agent: &mut AgentDefinition,
    options: &SessionOptions<'_>,
    redaction: RedactSecrets,
    spill: Option<SpillOutputs>,
) -> Result<AutoApprove> {
    extensions::register(config, &options.shared.mcp.servers).map_err(anyhow::Error::msg)?;
    let auto_approve = agent.permissions.auto_approval();
    // The agent's permissions and the frontend's bash policy each own the list (`rules_policy`).
    if let Some(rules) = rules_policy(agent, &options.shared.mcp.ask, auto_approve.clone()) {
        push_built_in(config, "agent_permissions", PolicyHook::new(Arc::new(rules)));
    }
    push_built_in(
        config,
        "continue_on_length",
        ContinueOnLength::new(agent.continue_on_length),
    );
    push_built_in(config, "redact_secrets", redaction);
    if let Some(spill) = spill {
        push_built_in(config, "spill_outputs", spill);
    }
    // The built-in tools skip repeated names; this guards tools registered here later.
    config.validate().map_err(anyhow::Error::msg)?;
    Ok(auto_approve)
}

/// Variables commands must not inherit: the API key, the other credentials and the headers
/// that authenticate to a trace collector. `env` would put them into the session file and
/// the context.
fn hidden_env(options: &SessionOptions<'_>) -> Vec<String> {
    std::iter::once(options.shared.settings.api_key_env.clone())
        .chain(options.shared.hidden_env.iter().cloned())
        .chain(options.shared.mcp.token_env.iter().cloned())
        .chain(otel::HEADER_VARIABLES.iter().map(std::string::ToString::to_string))
        .collect()
}

/// The values to redact of the hidden variables besides the key; an unset or empty variable
/// holds nothing.
fn hidden_values<'a>(options: &'a SessionOptions<'_>) -> impl Iterator<Item = String> + 'a {
    options
        .shared
        .hidden_env
        .iter()
        .chain(&options.shared.mcp.token_env)
        .filter_map(|name| env_value(name))
}

/// The agent's denied and asking tools and the MCP tools that ask, as one policy that allows
/// the rest; none without any of them.
///
/// Every policy that asks for bash gets the auto-approved commands: the strictest verdict of
/// all policies wins, so one that still asked would cancel the others' auto-approval.
fn rules_policy(agent: &mut AgentDefinition, mcp_ask: &[String], auto_approve: AutoApprove) -> Option<RulePolicy> {
    let denied = agent.permissions.denied_tools().into_iter().map(|tool| Rule {
        matcher: RuleMatcher::Tool(tool),
        decision: PolicyDecision::Deny("tool denied by the agent".into()),
    });
    let asked = std::mem::take(&mut agent.permissions.ask).into_iter().map(|tool| Rule {
        matcher: RuleMatcher::Tool(tool),
        decision: PolicyDecision::Ask("the agent requires tool approval".into()),
    });
    // An MCP server's tools ask unless permissions.allow lists them: annotations are hints.
    let mcp = mcp_ask.iter().map(|tool| Rule {
        matcher: RuleMatcher::Tool(tool.clone()),
        decision: PolicyDecision::Ask("MCP tool requires approval (not in permissions.allow)".into()),
    });
    let mut rules: Vec<Rule> = denied.chain(asked).chain(mcp).collect();
    if rules.is_empty() {
        return None;
    }
    rules.push(Rule {
        matcher: RuleMatcher::Any,
        decision: PolicyDecision::Allow,
    });
    Some(RulePolicy::new(rules).auto_approve(auto_approve))
}
