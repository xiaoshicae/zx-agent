//! The assembly of one session, shared by run, chat, serve and acp: the profile with the
//! command line over it, the provider, the tools, the session file and the hooks every mode
//! registers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use zx_agent::AgentSession;
use zx_agent::agent::{EventSink, HookOptions, Hooks, LoopConfig, LoopSpec, PolicyDecision, Tool};
use zx_agent::ai::ModelMeta;
use zx_agent::ai::openai::{OpenAiCompatible, OpenAiConfig, SystemUpdates};
use zx_agent::ai::retry::RetryPolicy;
use zx_agent::compaction;
use zx_agent::continuation::ContinueOnLength;
use zx_agent::executor::Executor;
use zx_agent::extensions;
use zx_agent::otel;
use zx_agent::output_store::{self, OutputStore, ReadOutputTool, SpillOutputs};
use zx_agent::policy::{AutoApprove, CommandPolicy, PolicyHook, Rule, RuleMatcher, RulePolicy};
use zx_agent::profile::skills::{ReadSkillTool, TOOL_NAME};
use zx_agent::profile::{Builtin, Execution, Profile};
use zx_agent::prompt::{self, build_system_prompt};
use zx_agent::redact::RedactSecrets;
use zx_agent::runner::{Limits, Runner};
use zx_agent::session::{SessionHeader, SessionStore};
use zx_agent::tools::{self, BASH};

use super::execution::{check_session_outside, executor};
use super::{
    BaseArgs, BudgetArgs, ExecutionArg, RequestArgs, Shared, SystemUpdatesArg, TransportArgs, env_value, resolve_cwd,
};
use crate::sessions::{self, SessionFile};

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
    pub(crate) transport: &'a TransportArgs,
    pub(crate) request: &'a RequestArgs,
    pub(crate) budget: &'a BudgetArgs,
    pub(crate) model: &'a str,
    /// The working directory, over `--cwd`: a client's project directory. `None` keeps the
    /// command line's.
    pub(crate) cwd: Option<&'a Path>,
    pub(crate) file: SessionFile<'a>,
    pub(crate) default_profile: Builtin,
    /// Times to continue an answer cut off by the output token limit; `None` keeps the
    /// profile's value.
    pub(crate) continue_on_length: Option<u32>,
    pub(crate) shared: &'a Shared,
    /// Environment variables that hold credentials besides the API key, such as IM tokens:
    /// commands do not inherit them, and their values are redacted like the key's.
    pub(crate) hidden_env: &'a [String],
}

/// A configured session with the hooks every mode shares.
///
/// A mode adds only what is its own, such as chat's approval rule and interaction host, then
/// calls `into_session`.
pub(crate) struct PreparedSession {
    pub(crate) config: LoopConfig,
    retry: RetryPolicy,
    pub(crate) store: SessionStore,
    /// The profile's auto-approved commands, for frontend policies that also ask for bash.
    auto_approve: AutoApprove,
    /// The profile's `rules.approveAll`: nothing waits for a person.
    approve_all: bool,
    limits: Limits,
    compaction: compaction::Config,
    /// The selected profile's name, for a frontend to show.
    pub(crate) profile_name: String,
    /// The profile's auto-approved command prefixes, for a frontend to show.
    pub(crate) auto_approve_commands: Vec<String>,
}

impl PreparedSession {
    /// Ask a person before every bash command, except those the profile auto-approves, or
    /// none when the profile approves every call; other tools follow the profile's rules.
    /// Chat and serve both work this way.
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

/// Refuse --request-field values that --reasoning would also set: which one the server
/// gets would not be clear from the command.
fn check_reasoning_fields(request: &RequestArgs, fields: &serde_json::Map<String, serde_json::Value>) -> Result<()> {
    if request.reasoning.is_some()
        && let Some(field) = request
            .reasoning_format
            .fields()
            .iter()
            .find(|field| fields.contains_key(**field))
    {
        anyhow::bail!(
            "--request-field {field} conflicts with --reasoning, which sets it with --reasoning-format {}; use one of them",
            request.reasoning_format.as_str()
        );
    }
    Ok(())
}

/// `--request-field` values, each KEY=JSON, as the provider's extra fields.
fn request_fields(values: &[String]) -> Result<serde_json::Map<String, serde_json::Value>> {
    let mut fields = serde_json::Map::new();
    for value in values {
        let (field, json) = value
            .split_once('=')
            .filter(|(field, _)| !field.is_empty())
            .with_context(|| format!("--request-field {value:?} is not of the form KEY=JSON"))?;
        let parsed =
            serde_json::from_str(json).with_context(|| format!("--request-field {field}: the value is not JSON"))?;
        anyhow::ensure!(
            fields.insert(field.to_string(), parsed).is_none(),
            "--request-field {field} is given more than once"
        );
    }
    Ok(fields)
}

/// Assemble a session: startup reads, short session appends and, in container mode, starting
/// the container precede the run.
///
/// Configuration errors and a container that cannot start are reported before a new session
/// file is created.
pub(crate) async fn prepare(options: SessionOptions<'_>) -> Result<PreparedSession> {
    let (cwd, mut profile, models) = settle(&options)?;
    let context = profile.context_section(&cwd).context(
        "cannot load project instructions; --no-context skips AGENTS.md discovery and profile context files",
    )?;
    // Before the session file: invalid provider settings are configuration errors too.
    let (provider, api_key) = provider(&options)?;
    let mut host = host_tools(&mut profile, &cwd)?;
    let (session, executor, environment) = open_session(&options, &profile, &cwd).await?;
    let tools = assemble_tools(&profile, &options, &executor, &mut host)?;
    let policy = CommandPolicy::new(&profile.rules.allow).deny_also(&profile.rules.deny);
    let command_policy = command_policy_section(&policy, &tools);
    let system_prompt = build_system_prompt(
        &profile.instructions,
        &[
            (prompt::ENVIRONMENT, &environment),
            (prompt::COMMAND_POLICY, &command_policy),
            (prompt::PROJECT_CONTEXT, &context),
            (prompt::AVAILABLE_SKILLS, &host.catalog),
        ],
    );
    let mut config = LoopConfig::new(LoopSpec {
        provider: Arc::new(provider),
        model: options.model.to_string(),
        system_prompt,
        tools,
        cwd,
        max_tokens: options.request.max_tokens,
        reasoning: options.request.reasoning,
        sequential_tools: false,
    });
    push_built_in(&mut config, "command_policy", PolicyHook::new(Arc::new(policy)));
    // Each session's redaction owns its list: the hook reads the files again before every use.
    let redaction = RedactSecrets::new(api_key.into_iter().chain(hidden_values(&options)))
        .with_secret_files(options.shared.secret_files.clone());
    let auto_approve = register_hooks(&mut config, &mut profile, &options, redaction, host.spill.take())?;
    config.models = models;
    config.cache_key = options.request.prompt_cache_key.then(|| session.header().id.clone());
    Ok(PreparedSession {
        config,
        retry: RetryPolicy {
            max_retries: options.transport.max_retries,
            ..RetryPolicy::default()
        },
        store: session,
        auto_approve,
        approve_all: profile.rules.approve_all,
        limits: profile.limits,
        compaction: profile.compaction,
        profile_name: profile.name,
        auto_approve_commands: profile.rules.auto_approve,
    })
}

/// The working directory, the profile with the command line over it, and what is known about
/// the models. A cost budget needs the model's prices; container execution uses the
/// directory's real path.
fn settle(options: &SessionOptions<'_>) -> Result<(PathBuf, Profile, HashMap<String, ModelMeta>)> {
    let cwd = resolve_cwd(options.cwd.or(options.base.cwd.as_deref()))?;
    let mut profile = load_profile(options)?;
    let models = model_meta(&mut profile, options);
    // Without prices every response costs nothing, and the budget would never stop a run.
    anyhow::ensure!(
        profile.limits.cost_budget.is_none() || models.get(options.model).is_some_and(|meta| meta.cost.is_some()),
        "a cost budget needs the prices of the model {:?}: add them to the profile's [[models]]",
        options.model
    );
    // The container mounts the workspace at its real path, which tools must use as well.
    let cwd = match profile.execution {
        Execution::Local => cwd,
        Execution::Container => cwd
            .canonicalize()
            .with_context(|| format!("cannot resolve {}", cwd.display()))?,
    };
    Ok((cwd, profile, models))
}

/// The built-in tools the profile enables, running on `executor`, and the tools of this
/// machine.
fn assemble_tools(
    profile: &Profile,
    options: &SessionOptions<'_>,
    executor: &Arc<dyn Executor>,
    host: &mut HostTools,
) -> Result<Vec<Arc<dyn Tool>>> {
    let built_in: Vec<String> = profile.tools.iter().filter(|name| !host_tool(name)).cloned().collect();
    let timeout = Duration::from_secs(options.transport.tool_timeout_secs);
    let mut tools = tools::builtin_tools(&built_in, timeout, executor)?;
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

/// The selected profile with the command line's settings over it, validated. Explicit values
/// replace the profile's; denied commands and context files add to it.
fn load_profile(options: &SessionOptions<'_>) -> Result<Profile> {
    let mut profile = Profile::load(options.base.profile.as_deref(), options.default_profile)?;
    if let Some(tools) = &options.base.tools {
        profile.tools = tools
            .iter()
            .map(|name| name.trim())
            .filter(|name| !name.is_empty())
            .map(String::from)
            .collect();
    }
    apply_budgets(&mut profile, options);
    apply_rules(&mut profile, options.base);
    apply_context(&mut profile, options);
    apply_execution(&mut profile, options.base)?;
    profile.validate()?;
    Ok(profile)
}

/// `--max-model-requests`, `--timeout-secs`, the token and cost budgets and
/// `--continue-on-length`; a zero timeout or budget removes the profile's.
fn apply_budgets(profile: &mut Profile, options: &SessionOptions<'_>) {
    if let Some(turns) = options.budget.max_model_requests {
        profile.limits.max_model_requests = Some(turns);
    }
    if let Some(secs) = options.budget.timeout_secs {
        profile.limits.timeout = (secs > 0).then(|| Duration::from_secs(secs));
    }
    if let Some(tokens) = options.budget.token_budget {
        profile.limits.token_budget = (tokens > 0).then_some(tokens);
    }
    if let Some(amount) = options.budget.cost_budget {
        profile.limits.cost_budget = (amount > 0.0).then_some(amount);
    }
    if let Some(count) = options.continue_on_length {
        profile.continue_on_length = count;
    }
}

/// The command rules: an allowlist or auto-approval list replaces the profile's, denials add.
fn apply_rules(profile: &mut Profile, base: &BaseArgs) {
    if !base.allow_cmd.is_empty() {
        profile.rules.allow.clone_from(&base.allow_cmd);
    }
    profile.rules.deny.extend_from_slice(&base.deny_cmd);
    if !base.auto_approve_cmd.is_empty() {
        profile.rules.auto_approve.clone_from(&base.auto_approve_cmd);
    }
}

/// What the model is told and kept: the instructions, the project context and the summary
/// triggers.
fn apply_context(profile: &mut Profile, options: &SessionOptions<'_>) {
    let base = options.base;
    if let Some(instructions) = &options.shared.instructions {
        profile.instructions.clone_from(instructions);
    }
    if base.no_context {
        profile.context.discover = false;
        profile.context.files.clear();
    }
    profile.context.files.extend_from_slice(&base.context_files);
    if let Some(bytes) = base.compact_threshold_bytes {
        profile.compaction.trigger_bytes = (bytes > 0).then_some(bytes);
    }
    if base.no_auto_compact {
        profile.compaction.trigger_bytes = None;
        profile.compaction.trigger_tokens = None;
    }
}

/// Where commands run. An image needs container execution, and container execution an image.
fn apply_execution(profile: &mut Profile, base: &BaseArgs) -> Result<()> {
    if let Some(execution) = base.execution {
        profile.execution = match execution {
            ExecutionArg::Local => Execution::Local,
            ExecutionArg::Container => Execution::Container,
        };
    }
    if let Some(image) = &base.container_image {
        anyhow::ensure!(
            profile.execution == Execution::Container,
            "--container-image needs container execution: add --execution container, or set execution = \"container\" in the profile"
        );
        profile.container.image = Some(image.clone());
    }
    // The profile's own check says only what the file lacks; here the flag is the other way.
    anyhow::ensure!(
        profile.execution == Execution::Local || profile.container.image.is_some(),
        "execution = \"container\" needs container.image: set it in the profile, or give --container-image"
    );
    Ok(())
}

/// What is known about the models: the profile's entries, and the window given for this
/// model on the command line. With automatic summaries on, a known window adds a token
/// trigger below it.
fn model_meta(profile: &mut Profile, options: &SessionOptions<'_>) -> HashMap<String, ModelMeta> {
    let mut models = std::mem::take(&mut profile.models);
    if let Some(window) = options.base.context_window {
        models.entry(options.model.to_string()).or_default().context_window = Some(window);
    }
    if profile.compaction.trigger_bytes.is_some()
        && let Some(window) = models.get(options.model).and_then(|meta| meta.context_window)
    {
        profile.compaction.derive_trigger_tokens(window);
    }
    models
}

/// The provider of the command line's settings, and the API key it authenticates with.
fn provider(options: &SessionOptions<'_>) -> Result<(OpenAiCompatible, Option<String>)> {
    // Local servers (vLLM, Ollama, llama.cpp) often need no key, so a missing key is allowed.
    let api_key = env_value(&options.base.api_key_env);
    let extra_fields = request_fields(&options.request.request_fields)?;
    check_reasoning_fields(options.request, &extra_fields)?;
    let provider = OpenAiCompatible::new(OpenAiConfig {
        base_url: options.base.base_url.clone(),
        // Provider authentication and output filtering each need an owned copy.
        api_key: api_key.clone().into(),
        idle_timeout: Duration::from_secs(options.transport.idle_timeout_secs),
        max_response_bytes: options.request.max_response_bytes,
        max_sse_event_bytes: options.request.max_sse_event_bytes,
        system_updates: match options.base.system_updates {
            SystemUpdatesArg::Collapse => SystemUpdates::Collapse,
            SystemUpdatesArg::Inline => SystemUpdates::Inline,
        },
        extra_fields,
        fill_missing_reasoning: options.request.fill_missing_reasoning,
        reasoning_format: options.request.reasoning_format,
        headers: options.transport.headers.clone(),
        prompt_cache_key: options.request.prompt_cache_key,
        ..OpenAiConfig::default()
    })?;
    Ok((provider, api_key))
}

/// Whether `name` is a tool of this machine rather than of the executor: read_skill and
/// read_output read files here, whatever executor runs commands.
fn host_tool(name: &str) -> bool {
    name == TOOL_NAME || name == output_store::TOOL_NAME
}

/// The tools of this machine (see [`host_tool`]) that the profile enables, with the skill
/// catalog for the prompt and the hook that saves long results.
struct HostTools {
    tools: Vec<Arc<dyn Tool>>,
    catalog: String,
    spill: Option<SpillOutputs>,
}

fn host_tools(profile: &mut Profile, cwd: &Path) -> Result<HostTools> {
    let mut host = HostTools {
        tools: Vec::new(),
        catalog: String::new(),
        spill: None,
    };
    if profile.tools.iter().any(|name| name == TOOL_NAME) && !profile.skills.is_empty() {
        let tool = ReadSkillTool::new(std::mem::take(&mut profile.skills))?;
        host.catalog = tool.catalog();
        host.tools.push(Arc::new(tool));
    }
    if profile.tools.iter().any(|name| name == output_store::TOOL_NAME) {
        let store = OutputStore::new(cwd, std::mem::take(&mut profile.tool_output))?;
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
async fn open_session(
    options: &SessionOptions<'_>,
    profile: &Profile,
    cwd: &Path,
) -> Result<(SessionStore, Arc<dyn Executor>, String)> {
    if profile.execution == Execution::Container {
        check_session_outside(&options.file, cwd)?;
    }
    let header = SessionHeader::new(cwd, options.model);
    let opened = sessions::open_existing(&options.file)?;
    let session_id = opened
        .as_ref()
        .map_or_else(|| header.id.clone(), |session| session.header().id.clone());
    let (executor, environment) = executor(profile, cwd, &session_id, hidden_env(options)).await?;
    let session = match opened {
        Some(session) => session,
        None => sessions::create(options.file, header)?,
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
/// profile's rules, continuation, `redaction` and `spill`; returns the auto-approved commands
/// for the frontend's own bash policy.
///
/// Each hook declares its stage, so the policies run first and redaction runs before output
/// is saved, whatever is registered later.
fn register_hooks(
    config: &mut LoopConfig,
    profile: &mut Profile,
    options: &SessionOptions<'_>,
    redaction: RedactSecrets,
    spill: Option<SpillOutputs>,
) -> Result<AutoApprove> {
    extensions::register(config, &options.shared.mcp.servers).map_err(anyhow::Error::msg)?;
    let auto_approve = profile.rules.auto_approval();
    // The profile's rules and the frontend's bash policy each own the list (`rules_policy`).
    if let Some(rules) = rules_policy(profile, &options.shared.mcp.ask, auto_approve.clone()) {
        push_built_in(config, "profile_rules", PolicyHook::new(Arc::new(rules)));
    }
    push_built_in(
        config,
        "continue_on_length",
        ContinueOnLength::new(profile.continue_on_length),
    );
    push_built_in(config, "redact_secrets", redaction);
    if let Some(spill) = spill {
        push_built_in(config, "spill_outputs", spill);
    }
    // The built-in tools skip repeated names; this guards tools registered here later.
    config.spec.validate().map_err(anyhow::Error::msg)?;
    Ok(auto_approve)
}

/// Variables commands must not inherit: the API key, the other credentials and the headers
/// that authenticate to a trace collector. `env` would put them into the session file and
/// the context.
fn hidden_env(options: &SessionOptions<'_>) -> Vec<String> {
    std::iter::once(options.base.api_key_env.clone())
        .chain(options.hidden_env.iter().cloned())
        .chain(options.shared.mcp.token_env.iter().cloned())
        .chain(otel::HEADER_VARIABLES.iter().map(std::string::ToString::to_string))
        .collect()
}

/// The values to redact of the hidden variables besides the key; an unset or empty variable
/// holds nothing.
fn hidden_values<'a>(options: &'a SessionOptions<'_>) -> impl Iterator<Item = String> + 'a {
    options
        .hidden_env
        .iter()
        .chain(&options.shared.mcp.token_env)
        .filter_map(|name| env_value(name))
}

/// The profile's tool rules and the MCP tools that ask, as one policy that allows the rest;
/// none without rules.
///
/// Every policy that asks for bash gets the auto-approved commands: the strictest verdict of
/// all policies wins, so one that still asked would cancel the others' auto-approval.
fn rules_policy(profile: &mut Profile, mcp_ask: &[String], auto_approve: AutoApprove) -> Option<RulePolicy> {
    let denied = std::mem::take(&mut profile.rules.deny_tools)
        .into_iter()
        .map(|tool| Rule {
            matcher: RuleMatcher::Tool(tool),
            decision: PolicyDecision::Deny("tool denied by profile".into()),
        });
    let asked = std::mem::take(&mut profile.rules.ask_tools)
        .into_iter()
        .map(|tool| Rule {
            matcher: RuleMatcher::Tool(tool),
            decision: PolicyDecision::Ask("profile requires tool approval".into()),
        });
    // An MCP server's tools ask unless its allowTools lists them: annotations are hints.
    let mcp = mcp_ask.iter().map(|tool| Rule {
        matcher: RuleMatcher::Tool(tool.clone()),
        decision: PolicyDecision::Ask("MCP tool requires approval (not in the server's allowTools)".into()),
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
