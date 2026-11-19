//! The agent a host builds: the agent file with the host's [`Overrides`] over it, in the
//! environment of the settings, with the provider, the policies and the hooks every agent
//! registers, and the sessions the host creates or opens with it.
//!
//! The agent file is loaded here, once for each agent a host builds, and everything that
//! depends on it is decided here too, such as which MCP tools ask. What belongs to one
//! session is in `setup`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::{AgentDefinition, Config, READ_SKILL, cost_budget};
use crate::otel;
use zx_agent::ai::openai::{OpenAiCompatible, OpenAiConfig};
use zx_agent::ai::{Provider, SystemPrompt};
use zx_agent::builtin::continuation::ContinueOnLength;
use zx_agent::builtin::policy::{AutoApprove, CommandPolicy, Rule, RuleMatcher, RulePolicy};
use zx_agent::builtin::redact::RedactSecrets;
use zx_agent::builtin::tools::BASH;
use zx_agent::hooks::HookChain;
use zx_agent::policy::{ApprovalMode, PolicyChain, PolicyDecision};
use zx_agent::tools::Tool;
use zx_agent::{Agent, AgentConfig, Session, SessionOptions};
use zx_util::env::env_value;

use super::settings::BASE_URL_ENV;
use super::setup::PerSession;
use super::skills::ReadSkillTool;
use super::{Application, Overrides, SessionPlace, resolve_cwd};

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
    /// be used, a cost budget without prices, invalid provider settings.
    pub(super) fn build(
        app: &Application,
        model: &str,
        place: &SessionPlace,
        approvals: Approvals<'_>,
    ) -> Result<Self> {
        let settings = &app.settings;
        let config = &settings.config;
        let mut agent = load_agent(app)?;
        let models = config.models.clone();
        // Without prices every response costs nothing, and the budget would never stop a run.
        anyhow::ensure!(
            agent.limits.cost_budget.is_none() || models.get(model).is_some_and(|meta| meta.cost.is_some()),
            "a cost budget needs the prices of the model {model:?}: add them to [[models]] in config.toml"
        );
        let (provider, api_key) = provider(config)?;
        let provider: Arc<dyn Provider> = Arc::new(provider);
        let skills = skills_tool(&mut agent)?;
        let (policies, approvals) = policies(app, &mut agent, approvals);
        let name = std::mem::take(&mut agent.name);
        let auto_approve_commands = agent.permissions.allowed_commands();
        let role = AgentConfig {
            system_prompt: SystemPrompt::new(&agent.instructions),
            max_tokens: config.max_tokens,
            reasoning: config.reasoning,
            hooks: hooks(app, &agent, api_key),
            policies,
            approvals,
            extensions: app.mcp.servers.clone(),
            models,
            session_cache_key: config.provider.openai.body.prompt_cache_key,
            limits: agent.limits,
            retry: config.provider.retry.clone(),
            compaction: agent.compaction,
            storage: place.storage(),
            session_setup: Some(Arc::new(PerSession {
                tools: agent.tools,
                context: agent.context,
                skills,
                execution: config.execution,
                container: config.container.clone(),
                hidden_env: hidden_env(app),
                tool_timeout: settings.tool_timeout,
                tool_output: config.tool_output.clone(),
                place: place.clone(),
                file_queue: Arc::clone(&app.file_queue),
            })),
            ..AgentConfig::new(provider.clone(), model)
        };
        Ok(Self {
            agent: Agent::new(role)?,
            provider,
            name,
            auto_approve_commands,
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

    /// Open a stored session without writing diagnostics to the process's outputs.
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
    /// Startup reads and, in container mode, starting the container. A container that cannot
    /// start is reported before the session's file is created.
    pub async fn create(&self, cwd: Option<&Path>) -> Result<Session> {
        let options = SessionOptions::new(self.new_session_cwd(cwd)?);
        self.agent.create_session(options).await
    }
}

/// Refuse request fields that the reasoning level would also set: which one the server gets
/// would not be clear from the configuration.
fn check_reasoning_fields(config: &Config) -> Result<()> {
    let body = &config.provider.openai.body;
    if config.reasoning.is_some()
        && let Some(field) = body
            .reasoning_format
            .fields()
            .iter()
            .find(|field| body.extra_fields.contains_key(**field))
    {
        anyhow::bail!(
            "provider.requestFields.{field} conflicts with reasoning, which sets it with provider.reasoningFormat {}; use one of them",
            body.reasoning_format
        );
    }
    Ok(())
}

/// The agent `app` runs, with the host's tools and budgets over it, validated: explicit
/// values replace the agent's.
fn load_agent(app: &Application) -> Result<AgentDefinition> {
    let config = &app.settings.config;
    let mut agent = AgentDefinition::load(app.builtin, app.settings.home.as_deref(), config)?;
    if let Some(tools) = &app.overrides.tools {
        agent.tools = tools
            .iter()
            .map(|name| name.trim())
            .filter(|name| !name.is_empty())
            .map(String::from)
            .collect();
    }
    apply_budgets(&mut agent, &app.overrides)?;
    agent.validate(config)?;
    Ok(agent)
}

/// The request limit, the time limit and the token and cost budgets of `budget` over the
/// agent's; a zero timeout or budget removes the agent's. A cost budget that an agent file
/// could not hold either is an error.
fn apply_budgets(agent: &mut AgentDefinition, budget: &Overrides) -> Result<()> {
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
        agent.limits.cost_budget = cost_budget(amount)?;
    }
    Ok(())
}

/// The provider of config.toml, and the API key it authenticates with.
fn provider(config: &Config) -> Result<(OpenAiCompatible, Option<String>)> {
    // Local servers (vLLM, Ollama, llama.cpp) often need no key, so a missing key is allowed.
    let api_key = env_value(&config.provider.api_key_env);
    check_reasoning_fields(config)?;
    // Provider authentication and output filtering each need an owned copy of the key.
    let provider = OpenAiCompatible::new(OpenAiConfig {
        api_key: api_key.clone().into(),
        ..config.provider.openai.clone()
    })
    .with_context(|| {
        format!("invalid [provider] settings in config.toml, or {BASE_URL_ENV} in place of its baseUrl")
    })?;
    Ok((provider, api_key))
}

/// read_skill with the catalog of its skills for the prompt, when the agent enables it and
/// has skills.
fn skills_tool(agent: &mut AgentDefinition) -> Result<Option<(Arc<dyn Tool>, String)>> {
    if agent.skills.is_empty() || !agent.tools.iter().any(|name| name == READ_SKILL) {
        return Ok(None);
    }
    let tool = ReadSkillTool::new(std::mem::take(&mut agent.skills))?;
    let catalog = tool.catalog();
    Ok(Some((Arc::new(tool), catalog)))
}

/// The policies every session of the agent asks, in this order: the command policy, the
/// policy of the agent's permissions with the MCP tools it does not allow, and, where a
/// person answers, the one that asks before bash commands. With them, who answers.
///
/// Every policy that asks for bash gets the auto-approved commands: the strictest verdict of
/// all policies wins, so one that still asked would cancel the others' auto-approval.
fn policies(app: &Application, agent: &mut AgentDefinition, approvals: Approvals<'_>) -> (PolicyChain, ApprovalMode) {
    let mut policies = PolicyChain::new();
    // The commands an agent allows run without asking, the others ask.
    let commands = CommandPolicy::new().deny_also(agent.permissions.denied_commands());
    policies.push("command_policy", Arc::new(commands));
    let auto_approve = agent.permissions.auto_approval();
    let mcp_ask = mcp_asking(agent, &app.mcp.names);
    if let Some(rules) = rules_policy(agent, &mcp_ask, auto_approve.clone()) {
        policies.push("agent_permissions", Arc::new(rules));
    }
    let reason = match approvals {
        Approvals::Nobody => return (policies, ApprovalMode::Deny),
        Approvals::Person(reason) => reason,
    };
    if !agent.permissions.approve_all {
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
        policies.push("bash_approval", Arc::new(bash));
    }
    (policies, ApprovalMode::Wait(app.settings.approval_timeout))
}

/// The hooks every session of the agent shares: continuation, redaction of the API key
/// `api_key` and the other credentials, and the trace export.
///
/// Each hook declares its stage, so redaction runs before a session's output is saved,
/// whatever is registered later. A built-in hook's name, which `hook_failed` events and
/// `[hook]` lines show, contains `_`, which MCP server names cannot, so it never clashes with
/// an extension's.
fn hooks(app: &Application, agent: &AgentDefinition, api_key: Option<String>) -> HookChain {
    let mut hooks = HookChain::new();
    hooks.push(
        "continue_on_length",
        Arc::new(ContinueOnLength::new(agent.continue_on_length)),
    );
    // The hook reads the secret files again before every use.
    let redaction =
        RedactSecrets::new(api_key.into_iter().chain(hidden_values(app))).with_secret_files(app.secret_files.clone());
    hooks.push("redact_secrets", Arc::new(redaction));
    // Observes every run of every session, whoever reads its events.
    if let Some(traces) = &app.traces {
        hooks.push("trace_export", Arc::new(traces.clone()));
    }
    hooks
}

/// Variables commands must not inherit: the API key, the other credentials and the headers
/// that authenticate to a trace collector. `env` would put them into the session file and
/// the context.
fn hidden_env(app: &Application) -> Vec<String> {
    std::iter::once(app.settings.config.provider.api_key_env.clone())
        .chain(app.hidden_env.iter().cloned())
        .chain(app.mcp.token_env.iter().cloned())
        .chain(otel::HEADER_VARIABLES.iter().map(std::string::ToString::to_string))
        .collect()
}

/// The values to redact of the hidden variables besides the key, the trace collector's headers
/// among them; an unset or empty variable holds nothing.
fn hidden_values(app: &Application) -> impl Iterator<Item = String> + '_ {
    app.hidden_env
        .iter()
        .chain(&app.mcp.token_env)
        .map(String::as_str)
        .chain(otel::HEADER_VARIABLES)
        .filter_map(env_value)
}

/// The MCP tools, of `names`, that ask a person: those not in the agent's `permissions.allow`,
/// and none when the agent approves every call, since the container is the sandbox.
fn mcp_asking(agent: &AgentDefinition, names: &[String]) -> Vec<String> {
    if agent.permissions.approve_all {
        return Vec::new();
    }
    let allowed = agent.permissions.allowed_tools();
    names.iter().filter(|name| !allowed.contains(name)).cloned().collect()
}

/// The agent's denied and asking tools and the MCP tools that ask, as one policy that allows
/// the rest; none without any of them.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> Config {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        Config::load(&path).unwrap()
    }

    #[test]
    fn request_fields_that_the_reasoning_level_sets_are_refused() {
        for (text, expected) in [
            (
                "reasoning = 'low'\n[provider]\nrequestFields = { reasoning_effort = 'high' }",
                "provider.requestFields.reasoning_effort conflicts with reasoning",
            ),
            (
                "reasoning = 'low'\n[provider]\nreasoningFormat = 'thinking'\n\
                 requestFields = { thinking = { type = 'enabled' } }",
                "provider.requestFields.thinking conflicts with reasoning",
            ),
        ] {
            let error = format!("{:#}", check_reasoning_fields(&config(text)).unwrap_err());
            assert!(error.contains(expected), "{text}: {error}");
        }
        // Without a reasoning level, the field is the only one that sets it.
        check_reasoning_fields(&config("[provider]\nrequestFields = { reasoning_effort = 'high' }")).unwrap();
    }
}
