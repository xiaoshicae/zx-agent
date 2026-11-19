//! The agent a host builds: the agent file with the host's [`Overrides`] over it, in the
//! environment of the settings, with the provider, the policies and the hooks every agent
//! registers, the host's own parts, and the sessions the host creates or opens with it.
//!
//! The agent file is loaded here, once for each agent a host builds, and everything that
//! depends on it is decided here too, such as which of the host's tools it enables and which
//! tools of extensions ask. What belongs to one session is in `setup`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

use crate::config::{
    AgentDefinition, HostTools, ModelRequestLimit, Permissions, READ_SKILL, RawLimits, Skill, known_tool,
};
use crate::otel;
use zx_agent::ai::{PREAMBLE, Provider, SystemPrompt};
use zx_agent::builtin::continuation::ContinueOnLength;
use zx_agent::builtin::policy::{AutoApprove, CommandPolicy, Rule, RuleMatcher, RulePolicy};
use zx_agent::builtin::redact::RedactSecrets;
use zx_agent::builtin::tools::BASH;
use zx_agent::hooks::HookChain;
use zx_agent::policy::{ApprovalMode, PolicyChain, PolicyDecision};
use zx_agent::tools::Tool;
use zx_agent::{Agent, AgentConfig, Session, SessionOptions};
use zx_util::env::env_value;

use super::execution::ExecutorFactory;
use super::provider::{BuiltProvider, ProviderInput};
use super::setup::{self, PerSession, SPILL_OUTPUTS};
use super::skills::ReadSkillTool;
use super::{Application, Overrides, SessionPlace, resolve_cwd};

/// The names of the hooks and policies the assembly registers, which `hook_failed` events,
/// traces and policy errors show.
const CONTINUE_ON_LENGTH: &str = "continue_on_length";
const REDACT_SECRETS: &str = "redact_secrets";
const TRACE_EXPORT: &str = "trace_export";
const COMMAND_POLICY: &str = "command_policy";
const AGENT_PERMISSIONS: &str = "agent_permissions";
const BASH_APPROVAL: &str = "bash_approval";
/// The names of the assembly's hooks, the one each session's store of long results among
/// them: a host's hook may not take them.
const BUILT_IN_HOOKS: [&str; 4] = [CONTINUE_ON_LENGTH, REDACT_SECRETS, TRACE_EXPORT, SPILL_OUTPUTS];
/// The names of the assembly's policies: a host's policy may not take them.
const BUILT_IN_POLICIES: [&str; 3] = [COMMAND_POLICY, AGENT_PERMISSIONS, BASH_APPROVAL];

/// Who answers when a tool call of the agent's sessions needs approval.
#[derive(Clone, Copy)]
pub enum Approvals<'a> {
    /// Nobody, for runs that nobody attends: a call that a policy asks about is denied.
    Nobody,
    /// A person, within the approval timeout of config.toml. Every bash command asks first,
    /// for this reason, except those the agent auto-approves, or none when the agent
    /// approves every call; other tools follow the agent's permissions.
    Person(&'a str),
}

/// An agent configured from config.toml and its agent file, with what the host says over
/// them, and the [`SessionPlace`] where its sessions are kept; with what a host shows about
/// it, such as its name and the commands it runs without asking.
pub struct ConfiguredAgent {
    agent: Agent,
    provider: Arc<dyn Provider>,
    name: String,
    auto_approve_commands: Vec<String>,
    place: SessionPlace,
    /// Where new sessions work unless a request names a directory: [`Overrides::cwd`].
    cwd: Option<PathBuf>,
}

impl ConfiguredAgent {
    /// The agent of `app` that asks `model` and keeps its sessions in `place`, sharing what
    /// `app` read and connected. Startup reads; no session exists yet.
    ///
    /// Configuration errors are reported here: an agent file or a tool setting that cannot
    /// be used, a cost budget without prices, invalid provider settings, a provider or an
    /// executor that no factory is registered for, approving every call where commands run
    /// on this machine, and a part of the host's whose name another part goes by.
    pub(super) fn build(
        app: &Application,
        model: &str,
        place: &SessionPlace,
        approvals: Approvals<'_>,
    ) -> Result<Self> {
        let settings = &app.settings;
        let config = &settings.config;
        let host = host_tools(app);
        check_names(app, &host)?;
        let AgentDefinition {
            name,
            instructions,
            tools: enabled,
            permissions,
            limits,
            continue_on_length,
            context,
            skills,
            compaction,
        } = load_agent(app, &host)?;
        let models = config.models.clone();
        // Without prices every response costs nothing, and the budget would never stop a run.
        anyhow::ensure!(
            limits.cost_budget.is_none() || models.get(model).is_some_and(|meta| meta.cost.is_some()),
            "a cost budget needs the prices of the model {model:?}: add them to [[models]] in config.toml"
        );
        let executor_factory = executor_factory(app, &permissions)?;
        let (built, key_env) = provider(app)?;
        let hidden_env = hidden_env(app, key_env);
        let (policies, approvals) = policies(app, &permissions, &host, approvals);
        let hooks = hooks(app, continue_on_length, &hidden_env);
        let skills = skills_tool(skills, &enabled);
        // The host's tools that the agent enables, as it enables the built-in ones.
        let tools = app
            .tools
            .iter()
            .filter(|tool| enabled.iter().any(|name| name == tool.name()))
            .cloned()
            .collect();
        let role = AgentConfig {
            system_prompt: SystemPrompt::new(&instructions),
            tools,
            max_tokens: config.max_tokens,
            reasoning: config.reasoning,
            hooks,
            policies,
            approvals,
            extensions: app.mcp.servers.iter().chain(&app.extensions).cloned().collect(),
            models,
            session_cache_key: built.session_cache_key,
            limits,
            retry: config.provider.retry.clone(),
            compaction,
            summarizer: app.summarizer.clone(),
            storage: place.storage(),
            session_setup: Some(Arc::new(PerSession {
                tools: enabled,
                context,
                skills,
                executor_factory,
                hidden_env,
                tool_timeout: settings.tool_timeout,
                tool_output: config.tool_output.clone(),
                place: place.clone(),
                file_queue: Arc::clone(&app.file_queue),
                host_sections: app.host_sections.clone(),
            })),
            ..AgentConfig::new(Arc::clone(&built.provider), model)
        };
        Ok(Self {
            agent: Agent::new(role)?,
            provider: built.provider,
            name,
            auto_approve_commands: permissions.allowed_commands(),
            place: place.clone(),
            cwd: app.overrides.cwd.clone(),
        })
    }

    /// The assembled kernel agent, for sending input, listing and closing sessions.
    pub fn agent(&self) -> &Agent {
        &self.agent
    }

    /// The configured role's name, for a host to display.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The provider, for a host that switches the model of a session.
    pub fn provider(&self) -> &Arc<dyn Provider> {
        &self.provider
    }

    /// The agent's auto-approved command prefixes, for a host to display.
    pub fn auto_approve_commands(&self) -> &[String] {
        &self.auto_approve_commands
    }

    /// Where the agent keeps its sessions, for a host that lists them or shows their files.
    pub fn place(&self) -> &SessionPlace {
        &self.place
    }

    /// Open a stored session; when it cannot be opened, the error names its file.
    pub async fn open(&self, id: &str) -> Result<Session> {
        match self.agent.open_session(id).await {
            Ok(session) => Ok(session),
            Err(error) => Err(self.place.naming_file(id, error).await),
        }
    }

    /// Where a new session works: `cwd` when a request names a project directory, else
    /// [`Overrides::cwd`], else the current directory, by its real path.
    ///
    /// Fails when that directory cannot be resolved or is not a directory.
    pub fn new_session_cwd(&self, cwd: Option<&Path>) -> Result<PathBuf> {
        resolve_cwd(cwd.or(self.cwd.as_deref()))
    }

    /// Create a session in `cwd`, otherwise in [`Overrides::cwd`] or the process's current
    /// directory.
    ///
    /// Reads the project's instructions and starts the session's executor, such as a
    /// container; one that cannot start is reported before the session's file is created.
    pub async fn create(&self, cwd: Option<&Path>) -> Result<Session> {
        let options = SessionOptions::new(self.new_session_cwd(cwd)?);
        self.agent.create_session(options).await
    }
}

/// The names of the host's own tools and of its extensions' tools, which the agent may name.
fn host_tools(app: &Application) -> HostTools {
    HostTools {
        own: app.tools.iter().map(|tool| tool.name().to_string()).collect(),
        extensions: app
            .extensions
            .iter()
            .flat_map(|extension| extension.tools())
            .map(|tool| tool.name().to_string())
            .collect(),
    }
}

/// Refuse a part of the host's whose name another part goes by already. A provider or an
/// executor may not be registered under a taken name. A tool of the host or of its
/// extensions may not be named like a built-in tool, which a session gets only when it is
/// made; tools that share a name otherwise, and extensions that do, the kernel refuses as the
/// agent is made. Extensions name their own hooks and policies, so an extension may not be
/// named like a built-in hook or policy; a host's hook may not be named like a built-in hook,
/// an extension or another of its hooks, and a policy likewise among policies: the kernel's
/// chains name hooks and policies apart. The host's prompt sections are added
/// beside the assembly's, so none may be named like one of those, the agent's instructions
/// or another of the host's.
fn check_names(app: &Application, host: &HostTools) -> Result<()> {
    app.providers.check_names().map_err(anyhow::Error::msg)?;
    app.executors.check_names().map_err(anyhow::Error::msg)?;
    for name in host.own.iter().chain(&host.extensions) {
        anyhow::ensure!(
            !known_tool(name),
            "the host's tool {name:?} has the name of a built-in tool"
        );
    }
    let mut hooks: HashSet<&str> = BUILT_IN_HOOKS.into();
    let mut policies: HashSet<&str> = BUILT_IN_POLICIES.into();
    for extension in app.mcp.servers.iter().chain(&app.extensions) {
        let name = extension.name();
        anyhow::ensure!(
            !BUILT_IN_HOOKS.contains(&name) && !BUILT_IN_POLICIES.contains(&name),
            "the extension {name:?} has the name of a built-in hook or policy"
        );
        // An extension's hooks and policies go by its name.
        hooks.insert(name);
        policies.insert(name);
    }
    for (name, _) in &app.hooks {
        anyhow::ensure!(
            hooks.insert(name),
            "the host's hook {name:?} has a name that a built-in hook, an extension or another of the host's hooks goes by"
        );
    }
    for (name, _) in &app.policies {
        anyhow::ensure!(
            policies.insert(name),
            "the host's policy {name:?} has a name that a built-in policy, an extension or another of the host's policies goes by"
        );
    }
    let mut sections: HashSet<&str> = setup::SECTION_NAMES.into_iter().chain([PREAMBLE]).collect();
    for (name, _) in &app.host_sections {
        anyhow::ensure!(
            sections.insert(name),
            "the host's prompt section {name:?} has a name that the agent's instructions, a section of the assembly or another of the host's goes by"
        );
    }
    Ok(())
}

/// The factory of the executor config.toml names in `execution`. Fails when there is none,
/// and when the agent's `permissions` approve every call but the factory does not isolate
/// commands.
fn executor_factory(app: &Application, permissions: &Permissions) -> Result<Arc<dyn ExecutorFactory>> {
    let name = &app.settings.config.execution;
    let factory = app
        .executors
        .get(name)
        .map_err(|error| anyhow::anyhow!("execution: {error}"))?;
    anyhow::ensure!(
        !permissions.approve_all || factory.isolates(),
        "permissions.approveAll needs execution = \"container\" in config.toml, or another executor that isolates commands: without a sandbox, every command would run on this machine unasked"
    );
    Ok(Arc::clone(factory))
}

/// The provider of config.toml, built by the factory registered for its `provider.api`, and
/// the variable that holds its API key.
fn provider(app: &Application) -> Result<(BuiltProvider, &str)> {
    let config = &app.settings.config;
    let factory = app
        .providers
        .get(&config.provider.api)
        .map_err(|error| anyhow::anyhow!("provider.api: {error}"))?;
    let key_env = config.provider.api_key_env.as_deref().unwrap_or(factory.api_key_env());
    let built = factory.build(ProviderInput {
        options: &config.provider.options,
        // Local servers (vLLM, Ollama, llama.cpp) often need no key, so a missing key is allowed.
        api_key: env_value(key_env),
        reasoning: config.reasoning,
    })?;
    Ok((built, key_env))
}

/// The agent `app` runs, with the host's tools and budgets over it, validated: explicit
/// values replace the agent's, and its tools may be the `host`'s.
fn load_agent(app: &Application, host: &HostTools) -> Result<AgentDefinition> {
    let config = &app.settings.config;
    let mut agent = AgentDefinition::load(app.builtin, app.settings.home.as_deref(), config, host)?;
    if let Some(tools) = &app.overrides.tools {
        agent.tools = tools
            .iter()
            .map(|name| name.trim())
            .filter(|name| !name.is_empty())
            .map(String::from)
            .collect();
    }
    budgets(&app.overrides).apply(&mut agent.limits)?;
    agent.validate(config, host)?;
    Ok(agent)
}

/// The request limit, the time limit and the token and cost budgets of `overrides`, laid over
/// the agent's as the `[limits]` of an agent file are.
fn budgets(overrides: &Overrides) -> RawLimits {
    RawLimits {
        max_model_requests: overrides.max_model_requests.map(ModelRequestLimit::Count),
        timeout_secs: overrides.timeout_secs,
        token_budget: overrides.token_budget,
        cost_budget: overrides.cost_budget,
    }
}

/// read_skill with the catalog of its `skills` for the prompt, when the agent has skills and
/// its `enabled` tools include it.
fn skills_tool(skills: Vec<Skill>, enabled: &[String]) -> Option<(Arc<dyn Tool>, String)> {
    if skills.is_empty() || !enabled.iter().any(|name| name == READ_SKILL) {
        return None;
    }
    let tool = ReadSkillTool::new(skills);
    let catalog = tool.catalog();
    Some((Arc::new(tool), catalog))
}

/// The policies every session of the agent asks, in this order: the command policy, the
/// policy of the agent's `permissions` with the tools of MCP servers and of the `host`'s
/// extensions that they do not allow, the host's own, and, where a person answers, the one
/// that asks before bash commands. With them, who answers.
///
/// Every policy that asks for bash gets the auto-approved commands: the strictest decision of
/// all policies wins, so one that still asked would cancel the others' auto-approval.
fn policies(
    app: &Application,
    permissions: &Permissions,
    host: &HostTools,
    approvals: Approvals<'_>,
) -> (PolicyChain, ApprovalMode) {
    let mut policies = PolicyChain::new();
    let commands = CommandPolicy::new().deny_also(permissions.denied_commands());
    policies.push(COMMAND_POLICY, Arc::new(commands));
    let auto_approve = permissions.auto_approval();
    let extension_tools: Vec<String> = app.mcp.names.iter().chain(&host.extensions).cloned().collect();
    let asking = asking_tools(permissions, &extension_tools);
    if let Some(rules) = rules_policy(permissions, &asking, auto_approve.clone()) {
        policies.push(AGENT_PERMISSIONS, Arc::new(rules));
    }
    for (name, policy) in &app.policies {
        policies.push(name.as_str(), Arc::clone(policy));
    }
    let reason = match approvals {
        Approvals::Nobody => return (policies, ApprovalMode::Deny),
        Approvals::Person(reason) => reason,
    };
    if !permissions.approve_all {
        let bash = RulePolicy::new(vec![
            Rule {
                matcher: RuleMatcher::Tool(BASH.into()),
                decision: PolicyDecision::Ask(reason.into()),
            },
            Rule {
                matcher: RuleMatcher::Any,
                decision: PolicyDecision::Allow,
            },
        ])
        .auto_approve(auto_approve);
        policies.push(BASH_APPROVAL, Arc::new(bash));
    }
    (policies, ApprovalMode::Wait(app.settings.approval_timeout))
}

/// The hooks every session of the agent shares: continuation of up to `continue_on_length`
/// cut responses, redaction of the values of the `hidden` variables and of the secret files,
/// the trace export, and the host's own.
///
/// Each hook declares its stage, so redaction runs before a session's output is saved,
/// whatever is registered later. A built-in hook's name, which `hook_failed` events and
/// `[hook]` lines show, is no extension's: [`check_names`] refuses one named so.
fn hooks(app: &Application, continue_on_length: u32, hidden: &[String]) -> HookChain {
    let mut hooks = HookChain::new();
    hooks.push(CONTINUE_ON_LENGTH, Arc::new(ContinueOnLength::new(continue_on_length)));
    // An unset or empty variable holds nothing to redact. The hook reads the secret files
    // again before every use.
    let values = hidden.iter().map(String::as_str).filter_map(env_value);
    let redaction = RedactSecrets::new(values).with_secret_files(app.secret_files.clone());
    hooks.push(REDACT_SECRETS, Arc::new(redaction));
    // Observes every run of every session, whoever reads its events.
    if let Some(traces) = &app.traces {
        hooks.push(TRACE_EXPORT, Arc::new(traces.clone()));
    }
    for (name, hook) in &app.hooks {
        hooks.push(name.as_str(), Arc::clone(hook));
    }
    hooks
}

/// Variables commands must not inherit, whose values are redacted: the API key's `key_env`,
/// the other credentials and the headers that authenticate to a trace collector. `env` would
/// put them into the session file and the context.
fn hidden_env(app: &Application, key_env: &str) -> Vec<String> {
    std::iter::once(key_env.to_string())
        .chain(app.hidden_env.iter().cloned())
        .chain(app.mcp.token_env.iter().cloned())
        .chain(otel::HEADER_ENVS.iter().map(std::string::ToString::to_string))
        .collect()
}

/// The tools of `names`, those of MCP servers and of the host's extensions, that ask a person:
/// those not in `permissions.allow`, and none when the agent approves every call, since the
/// container is the sandbox.
fn asking_tools(permissions: &Permissions, names: &[String]) -> Vec<String> {
    if permissions.approve_all {
        return Vec::new();
    }
    let allowed = permissions.allowed_tools();
    names.iter().filter(|name| !allowed.contains(name)).cloned().collect()
}

/// The denied and asking tools of the agent's `permissions` and the tools of extensions that
/// ask, MCP servers among them, as one policy that allows the rest; none without any of them.
fn rules_policy(permissions: &Permissions, extension_ask: &[String], auto_approve: AutoApprove) -> Option<RulePolicy> {
    let denied = permissions.denied_tools().into_iter().map(|tool| Rule {
        matcher: RuleMatcher::Tool(tool),
        decision: PolicyDecision::Deny("tool denied by the agent".into()),
    });
    let asked = permissions.ask.iter().map(|tool| Rule {
        matcher: RuleMatcher::Tool(tool.clone()),
        decision: PolicyDecision::Ask("the agent requires tool approval".into()),
    });
    // An MCP server's tools ask unless permissions.allow lists them: annotations are hints.
    // So do those of the host's extensions, which are treated as MCP servers are.
    let extension = extension_ask.iter().map(|tool| Rule {
        matcher: RuleMatcher::Tool(tool.clone()),
        decision: PolicyDecision::Ask("this tool requires approval (not in permissions.allow)".into()),
    });
    let mut rules: Vec<Rule> = denied.chain(asked).chain(extension).collect();
    if rules.is_empty() {
        return None;
    }
    rules.push(Rule {
        matcher: RuleMatcher::Any,
        decision: PolicyDecision::Allow,
    });
    Some(RulePolicy::new(rules).auto_approve(auto_approve))
}
