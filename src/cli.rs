//! Command-line settings and shared runtime assembly for run and chat.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};

use zx_agent::AgentSession;
use zx_agent::agent::{Decision, EventSink, LoopConfig, LoopSpec, Tool};
use zx_agent::ai::openai::{DEFAULT_MAX_RESPONSE_BYTES, OpenAiCompatible, OpenAiConfig, SystemUpdates};
use zx_agent::ai::retry::RetryPolicy;
use zx_agent::ai::sse::DEFAULT_MAX_EVENT_BYTES;
use zx_agent::compaction;
use zx_agent::executor::{ContainerConfig, ContainerExecutor, Executor, LocalExecutor};
use zx_agent::extensions::{ContinueOnLength, RedactSecrets};
use zx_agent::output_store::{self, OutputStore, ReadOutputTool, SpillOutputs};
use zx_agent::policy::{AutoApprove, CommandPolicy, PolicyHook, Rule, RuleMatcher, RulePolicy};
use zx_agent::profile::skills::{ReadSkillTool, TOOL_NAME};
use zx_agent::profile::{Builtin, ContainerSettings, Execution, Profile};
use zx_agent::prompt::{build_system_prompt, container_environment_section, environment_section};
use zx_agent::runner::{Limits, Runner};
use zx_agent::session::{SessionHeader, SessionStore};
use zx_agent::tools::{self, BASH};

/// Where Kubernetes mounts the Pod's ServiceAccount token, which kubectl in the Pod reads
/// (client-go `rest.InClusterConfig`). Commands can read it as well, so its value is
/// redacted; elsewhere the file does not exist and is skipped.
const SERVICE_ACCOUNT_TOKEN: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";

/// Comma-separated secret files to redact, added to `--secret-file`; deployments set it.
const SECRET_FILES_ENV: &str = "ZX_SECRET_FILES";

#[derive(Parser)]
#[command(name = "zx-agent", version, about = "Minimal general-purpose agent kernel")]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Run one task to completion without human interaction.
    Run(RunArgs),
    /// Start a conversation with streaming output and terminal approvals.
    Chat(ChatArgs),
    /// Answer chat messages from IM platforms, one session per thread, until stopped.
    Serve(ServeArgs),
}

#[derive(Args)]
pub(crate) struct RunArgs {
    /// Task description. Use "-" to read it from stdin.
    pub(crate) prompt: String,

    #[command(flatten)]
    pub(crate) base: BaseArgs,

    /// Maximum model requests for the run, failed attempts included. Defaults to the
    /// selected profile.
    #[arg(long)]
    pub(crate) max_turns: Option<u32>,

    /// Wall-clock limit for the whole run, in seconds. 0 disables it. Defaults to the
    /// selected profile.
    #[arg(long)]
    pub(crate) timeout_secs: Option<u64>,

    #[command(flatten)]
    pub(crate) transport: TransportArgs,

    /// Times to ask the model to continue an answer cut off by the output token limit.
    /// Each continuation counts against --max-turns. Defaults to the selected profile.
    #[arg(long)]
    pub(crate) continue_on_length: Option<u32>,

    #[command(flatten)]
    pub(crate) request: RequestArgs,

    /// text: progress on stderr, final answer on stdout. jsonl: one event per line on stdout.
    #[arg(long, value_enum, default_value_t = OutputMode::Text)]
    pub(crate) output: OutputMode,

    /// In jsonl mode, also emit streaming message_update events.
    #[arg(long)]
    pub(crate) deltas: bool,
}

#[derive(Args)]
pub(crate) struct ChatArgs {
    #[command(flatten)]
    pub(crate) base: BaseArgs,

    /// Maximum model requests per prompt. Defaults to the selected profile.
    #[arg(long)]
    pub(crate) max_turns: Option<u32>,

    /// Wall-clock limit per prompt, in seconds. 0 disables it. Defaults to the selected
    /// profile.
    #[arg(long)]
    pub(crate) timeout_secs: Option<u64>,

    /// How long an approval request waits for /approve or /deny, in seconds. An unanswered
    /// request is denied, and the task goes on without the call.
    #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..))]
    pub(crate) approval_timeout_secs: u64,

    #[command(flatten)]
    pub(crate) transport: TransportArgs,

    #[command(flatten)]
    pub(crate) request: RequestArgs,

    /// text: stream replies on stdout. jsonl: emit events and per-prompt reports.
    #[arg(long, value_enum, default_value_t = OutputMode::Text)]
    pub(crate) output: OutputMode,

    /// In jsonl mode, also emit streaming message_update events.
    #[arg(long)]
    pub(crate) deltas: bool,
}

#[derive(Args)]
pub(crate) struct ServeArgs {
    /// Serve configuration (TOML): platforms, who may use the bot, limits and the session
    /// directory. See README, section "IM 服务".
    #[arg(long)]
    pub(crate) config: PathBuf,

    #[command(flatten)]
    pub(crate) base: BaseArgs,

    /// Maximum model requests per prompt. Defaults to the selected profile.
    #[arg(long)]
    pub(crate) max_turns: Option<u32>,

    /// Wall-clock limit per prompt, in seconds. 0 disables it. Defaults to the selected
    /// profile.
    #[arg(long)]
    pub(crate) timeout_secs: Option<u64>,

    #[command(flatten)]
    pub(crate) transport: TransportArgs,

    #[command(flatten)]
    pub(crate) request: RequestArgs,
}

#[derive(Args)]
pub(crate) struct BaseArgs {
    /// Model id passed to the provider.
    #[arg(long, env = "ZX_MODEL")]
    pub(crate) model: String,

    /// OpenAI-compatible base URL (without /chat/completions).
    #[arg(long, env = "ZX_BASE_URL", default_value = "https://api.openai.com/v1")]
    pub(crate) base_url: String,

    /// Name of the environment variable holding the API key. The key itself is never
    /// accepted as an argument, so it does not appear in process listings.
    #[arg(long, default_value = "OPENAI_API_KEY")]
    pub(crate) api_key_env: String,

    /// How system prompt changes reach the model when a session is resumed. collapse: fold
    /// every version into the leading system message; works with any server. inline: send
    /// each change where it happened, which keeps the prompt cache and shows the model when
    /// the prompt changed, but needs a server that accepts system messages mid-conversation.
    #[arg(long, value_enum, default_value_t = SystemUpdatesArg::Collapse)]
    pub(crate) system_updates: SystemUpdatesArg,

    /// Replace the selected profile's instructions with the contents of this file. It is read
    /// once at startup and may be a pipe; the generated sections are still appended.
    #[arg(long)]
    pub(crate) system_prompt_file: Option<PathBuf>,

    /// Working directory for tools. Defaults to the current directory.
    #[arg(long)]
    pub(crate) cwd: Option<PathBuf>,

    /// Enabled tools, comma-separated. Defaults to the selected profile; explicit values replace it.
    #[arg(long, value_delimiter = ',')]
    pub(crate) tools: Option<Vec<String>>,

    /// Allowlisted command prefix for bash, e.g. --allow-cmd "kubectl get". Repeatable.
    /// When set, bash may only run '|' pipelines of allowlisted commands.
    #[arg(long = "allow-cmd")]
    pub(crate) allow_cmd: Vec<String>,

    /// Extra substring to reject in bash commands. Repeatable.
    #[arg(long = "deny-cmd")]
    pub(crate) deny_cmd: Vec<String>,

    /// Command prefix for bash that runs without asking where bash needs approval, as in
    /// chat, e.g. --auto-approve-cmd "kubectl get". Repeatable; replaces the profile's list.
    /// Commands with quotes, `$`, other shell syntax, the word "secret", a URL, a connection
    /// option or a kubectl option outside a read-only set still ask.
    #[arg(long = "auto-approve-cmd")]
    pub(crate) auto_approve_cmd: Vec<String>,

    /// File holding a secret to redact from tool output, such as another cluster's token, or a
    /// directory of such files (names starting with a dot are skipped). Read again before
    /// every redaction. Repeatable, and added to the files in the comma-separated
    /// ZX_SECRET_FILES. The Pod's ServiceAccount token is always redacted.
    #[arg(long = "secret-file", value_delimiter = ',')]
    pub(crate) secret_files: Vec<PathBuf>,

    /// Embedded profile name (troubleshoot, assistant) or path to a TOML file.
    #[arg(long)]
    pub(crate) profile: Option<String>,

    /// Additional project instructions file. Repeatable; relative to the launch directory.
    #[arg(long = "context-file", conflicts_with = "no_context")]
    pub(crate) context_files: Vec<PathBuf>,

    /// Disable automatic AGENTS.md discovery and profile context files.
    #[arg(long)]
    pub(crate) no_context: bool,

    /// Serialized context bytes that trigger a summary request. 0 disables it.
    /// This is a byte budget, not a model token-window guarantee.
    #[arg(long, conflicts_with = "no_auto_compact")]
    pub(crate) compact_threshold_bytes: Option<usize>,

    /// Disable automatic summaries; /compact remains available in chat.
    #[arg(long)]
    pub(crate) no_auto_compact: bool,

    /// Where bash and read_file run. local: this machine. container: an isolated Docker
    /// container without network access and with a read-only working directory.
    /// Defaults to the selected profile.
    #[arg(long, value_enum)]
    pub(crate) execution: Option<ExecutionArg>,

    /// Image for --execution container. It must already be present locally (docker pull).
    #[arg(long)]
    pub(crate) container_image: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ExecutionArg {
    Local,
    Container,
}

#[derive(Args)]
pub(crate) struct TransportArgs {
    /// Default bash command timeout, in seconds; at least 1.
    #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..))]
    pub(crate) tool_timeout_secs: u64,

    /// Longest wait for response headers or the next stream chunk, in seconds; at least 1.
    #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..))]
    pub(crate) idle_timeout_secs: u64,

    /// Retries for transient provider errors.
    #[arg(long, default_value_t = 3)]
    pub(crate) max_retries: u32,
}

#[derive(Args)]
pub(crate) struct RequestArgs {
    /// Output token limit per response. Reasoning models count their reasoning against it,
    /// so a small limit can leave the answer empty.
    #[arg(long)]
    pub(crate) max_tokens: Option<u32>,

    /// Maximum raw streaming response bytes per request, including SSE framing.
    #[arg(long, default_value_t = DEFAULT_MAX_RESPONSE_BYTES)]
    pub(crate) max_response_bytes: usize,

    /// Maximum bytes in one SSE event, including framing.
    #[arg(long, default_value_t = DEFAULT_MAX_EVENT_BYTES)]
    pub(crate) max_sse_event_bytes: usize,

    /// Session file (JSONL). Created if missing; an existing one is continued.
    #[arg(long)]
    pub(crate) session: Option<PathBuf>,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum SystemUpdatesArg {
    Collapse,
    Inline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum OutputMode {
    Text,
    Jsonl,
}

pub(crate) enum SessionFile<'a> {
    Memory,
    Open(&'a Path),
    Create(&'a Path),
}

impl<'a> SessionFile<'a> {
    pub(crate) fn from_path(path: Option<&'a Path>) -> Self {
        match path {
            Some(path) => Self::Open(path),
            None => Self::Memory,
        }
    }

    fn path(&self) -> Option<&'a Path> {
        match self {
            Self::Memory => None,
            Self::Open(path) | Self::Create(path) => Some(path),
        }
    }
}

/// `path` with its deepest existing ancestor resolved, so that a file that does not exist yet
/// is still compared by where it will be created.
fn resolve(path: &Path) -> Result<PathBuf> {
    let absolute = std::env::current_dir()?.join(path);
    let mut existing = absolute.as_path();
    let mut missing = Vec::new();
    while std::fs::symlink_metadata(existing).is_err() {
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name);
                existing = parent;
            }
            _ => break,
        }
    }
    let mut resolved = existing
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", existing.display()))?;
    resolved.extend(missing.iter().rev());
    Ok(resolved)
}

pub(crate) struct SessionOptions<'a> {
    pub(crate) base: &'a BaseArgs,
    pub(crate) transport: &'a TransportArgs,
    pub(crate) request: &'a RequestArgs,
    pub(crate) model: &'a str,
    pub(crate) file: SessionFile<'a>,
    pub(crate) default_profile: Builtin,
    pub(crate) budget: Budget,
    /// Text read once by [`read_instructions`]; `None` keeps the profile's instructions.
    pub(crate) instructions: Option<&'a str>,
    /// Environment variables that hold credentials besides the API key, such as IM tokens:
    /// commands do not inherit them, and their values are redacted like the key's.
    pub(crate) hidden_env: &'a [String],
}

/// Limits given on the command line. `None` keeps the selected profile's value.
pub(crate) struct Budget {
    pub(crate) max_turns: Option<u32>,
    pub(crate) timeout_secs: Option<u64>,
    pub(crate) continue_on_length: Option<u32>,
}

/// A configured session with the hooks every mode shares. A mode adds only what is its own,
/// such as chat's approval rule and interaction host, then calls `into_session`.
pub(crate) struct PreparedSession {
    pub(crate) config: LoopConfig,
    pub(crate) retry: RetryPolicy,
    pub(crate) store: SessionStore,
    /// The profile's auto-approved commands, for frontend policies that also ask for bash.
    pub(crate) auto_approve: AutoApprove,
    pub(crate) limits: Limits,
    pub(crate) compaction: compaction::Config,
}

/// Read `--system-prompt-file` once, at startup. As before profiles existed, it may be a
/// pipe and is not limited to the profile file budget, so it cannot be read again for a
/// later conversation.
pub(crate) fn read_instructions(base: &BaseArgs) -> Result<Option<String>> {
    base.system_prompt_file
        .as_deref()
        .map(|path| std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display())))
        .transpose()
}

/// Startup reads, short session appends and, in container mode, starting the container
/// precede the run. Configuration errors and a container that cannot start are reported
/// before a new session file is created.
pub(crate) async fn prepare(options: SessionOptions<'_>) -> Result<PreparedSession> {
    let cwd = match &options.base.cwd {
        Some(path) => path
            .canonicalize()
            .with_context(|| format!("invalid --cwd {}", path.display()))?,
        None => std::env::current_dir()?,
    };
    anyhow::ensure!(cwd.is_dir(), "--cwd is not a directory: {}", cwd.display());
    // Both sources count: a file named at the command line must not drop the ones a
    // deployment registered in the environment. A misspelt path would leave a secret
    // unredacted without a word, so each must exist at startup; one that disappears later is
    // skipped (RedactSecrets::with_secret_files).
    let from_environment: Vec<PathBuf> = match std::env::var_os(SECRET_FILES_ENV) {
        Some(value) => value
            .to_str()
            .with_context(|| format!("{SECRET_FILES_ENV} is not valid UTF-8"))?
            .split(',')
            .map(PathBuf::from)
            .collect(),
        None => Vec::new(),
    };
    let secret_files: Vec<PathBuf> = from_environment
        .into_iter()
        .chain(options.base.secret_files.iter().cloned())
        .filter(|path| !path.as_os_str().is_empty())
        .collect();
    for path in &secret_files {
        std::fs::metadata(path).with_context(|| {
            format!(
                "cannot use secret file {} (--secret-file or ZX_SECRET_FILES)",
                path.display()
            )
        })?;
    }
    let mut profile = Profile::load(options.base.profile.as_deref(), options.default_profile)?;
    if let Some(tools) = &options.base.tools {
        profile.tools = tools
            .iter()
            .map(|name| name.trim())
            .filter(|name| !name.is_empty())
            .map(String::from)
            .collect();
    }
    if let Some(turns) = options.budget.max_turns {
        profile.limits.max_turns = Some(turns);
    }
    if let Some(secs) = options.budget.timeout_secs {
        profile.limits.timeout = (secs > 0).then(|| Duration::from_secs(secs));
    }
    if let Some(count) = options.budget.continue_on_length {
        profile.continue_on_length = count;
    }
    if !options.base.allow_cmd.is_empty() {
        profile.rules.allow.clone_from(&options.base.allow_cmd);
    }
    profile.rules.deny.extend_from_slice(&options.base.deny_cmd);
    if !options.base.auto_approve_cmd.is_empty() {
        profile.rules.auto_approve.clone_from(&options.base.auto_approve_cmd);
    }
    if let Some(instructions) = options.instructions {
        profile.instructions = instructions.to_string();
    }
    if options.base.no_context {
        profile.context.discover = false;
        profile.context.files.clear();
    }
    profile.context.files.extend_from_slice(&options.base.context_files);
    if let Some(bytes) = options.base.compact_threshold_bytes {
        profile.compaction.trigger_bytes = (bytes > 0).then_some(bytes);
    }
    if options.base.no_auto_compact {
        profile.compaction.trigger_bytes = None;
    }
    if let Some(execution) = options.base.execution {
        profile.execution = match execution {
            ExecutionArg::Local => Execution::Local,
            ExecutionArg::Container => Execution::Container,
        };
    }
    if let Some(image) = &options.base.container_image {
        anyhow::ensure!(
            profile.execution == Execution::Container,
            "--container-image needs container execution: add --execution container, or set execution = \"container\" in the profile"
        );
        profile.container.image = Some(image.clone());
    }
    profile.validate()?;
    // The container mounts the workspace at its real path, which tools must use as well.
    let cwd = match profile.execution {
        Execution::Local => cwd,
        Execution::Container => cwd
            .canonicalize()
            .with_context(|| format!("cannot resolve {}", cwd.display()))?,
    };
    let context = profile.context_section(&cwd).context(
        "cannot load project instructions; --no-context skips AGENTS.md discovery and profile context files",
    )?;
    // Local servers (vLLM, Ollama, llama.cpp) often need no key, so a missing key is allowed.
    let api_key = std::env::var(&options.base.api_key_env)
        .ok()
        .filter(|key| !key.is_empty());
    // Before the session file: invalid provider settings are configuration errors too.
    let provider = OpenAiCompatible::new(OpenAiConfig {
        base_url: options.base.base_url.clone(),
        // Provider authentication and output filtering each need an owned copy.
        api_key: api_key.clone(),
        idle_timeout: Duration::from_secs(options.transport.idle_timeout_secs),
        max_response_bytes: options.request.max_response_bytes,
        max_sse_event_bytes: options.request.max_sse_event_bytes,
        system_updates: match options.base.system_updates {
            SystemUpdatesArg::Collapse => SystemUpdates::Collapse,
            SystemUpdatesArg::Inline => SystemUpdates::Inline,
        },
        ..OpenAiConfig::default()
    })?;
    // read_skill and read_output read files on this machine, whatever executor runs commands.
    let mut host_tools: Vec<Arc<dyn Tool>> = Vec::new();
    let catalog = if profile.tools.iter().any(|name| name == TOOL_NAME) && !profile.skills.is_empty() {
        let tool = ReadSkillTool::new(std::mem::take(&mut profile.skills))?;
        let catalog = tool.catalog();
        host_tools.push(Arc::new(tool));
        catalog
    } else {
        String::new()
    };
    let spill = if profile.tools.iter().any(|name| name == output_store::TOOL_NAME) {
        let store = OutputStore::new(&cwd, profile.tool_output)?;
        // The immutable path/config is used independently by the reading tool and writer hook.
        let hook = SpillOutputs::new(store.clone(), vec![output_store::TOOL_NAME.into(), TOOL_NAME.into()]);
        host_tools.push(Arc::new(ReadOutputTool::new(store)));
        Some(hook)
    } else {
        None
    };

    // The container reads the whole working directory: masking one path would leave hard links
    // to the session file and earlier session files beside it readable.
    if profile.execution == Execution::Container
        && let Some(path) = options.file.path()
    {
        anyhow::ensure!(
            !resolve(path)?.starts_with(&cwd),
            "--execution container needs the session file outside the working directory, which the container can read: {}; choose a --session path outside {}",
            path.display(),
            cwd.display()
        );
    }
    let header = SessionHeader::new(&cwd, options.model);
    // An existing session is opened first, for its ID. A new one is created only after the
    // container, if any, has started, so that a failed start leaves no session file behind.
    let opened = match options.file {
        SessionFile::Open(path) if path.exists() => Some(SessionStore::open(path)?),
        SessionFile::Memory | SessionFile::Open(_) | SessionFile::Create(_) => None,
    };
    let session_id = opened
        .as_ref()
        .map_or_else(|| header.id.clone(), |session| session.header().id.clone());
    let (executor, environment): (Arc<dyn Executor>, String) = match profile.execution {
        Execution::Local => (Arc::new(LocalExecutor), environment_section(&cwd)),
        Execution::Container => {
            let config = container_config(&profile.container, &cwd, &session_id)?;
            let user = config.user.clone();
            let container = ContainerExecutor::start(config)
                .await
                .context("cannot start the container for --execution container")?;
            let environment = container_environment_section(&cwd, container.image(), &user, container.platform());
            (Arc::new(container), environment)
        }
    };
    let session = match (opened, options.file) {
        (Some(session), _) => session,
        (None, SessionFile::Memory) => SessionStore::in_memory(header),
        (None, SessionFile::Open(path) | SessionFile::Create(path)) => SessionStore::create(path, header)?,
    };
    if session.recovered_tail_bytes() > 0 {
        eprintln!(
            "[session] recovered incomplete final record; removed {} bytes",
            session.recovered_tail_bytes()
        );
    }

    // Commands must not inherit the key: `env` would put it into the session file and the context.
    let hidden_env: Vec<String> = std::iter::once(options.base.api_key_env.clone())
        .chain(options.hidden_env.iter().cloned())
        .collect();
    let mut tools = tools::builtin_tools_with(
        &profile
            .tools
            .iter()
            .filter(|name| name.as_str() != TOOL_NAME && name.as_str() != output_store::TOOL_NAME)
            .cloned()
            .collect::<Vec<_>>(),
        Duration::from_secs(options.transport.tool_timeout_secs),
        &hidden_env,
        executor,
    )?;
    tools.extend(host_tools);
    let policy = CommandPolicy::new(&profile.rules.allow).deny_also(&profile.rules.deny);
    // The command policy only concerns bash.
    let command_policy = if tools.iter().any(|tool| tool.name() == BASH) {
        policy.prompt_section().unwrap_or_default()
    } else {
        String::new()
    };
    let system_prompt = build_system_prompt(
        &profile.instructions,
        &[
            ("environment", &environment),
            ("command_policy", &command_policy),
            ("project_context", &context),
            ("available_skills", &catalog),
        ],
    );

    let mut config = LoopConfig::new(LoopSpec {
        provider: Arc::new(provider),
        model: options.model.to_string(),
        system_prompt,
        tools,
        cwd,
        max_tokens: options.request.max_tokens,
        sequential_tools: false,
    });
    config.hooks.push(PolicyHook::new(Arc::new(policy)));
    // Every policy that asks for bash approval gets the list: the strictest verdict of all
    // policies wins, so one that still asked would cancel the others' auto-approval.
    let auto_approve = AutoApprove::new(&profile.rules.auto_approve);
    let mut rules = Vec::new();
    for tool in profile.rules.deny_tools {
        rules.push(Rule {
            matcher: RuleMatcher::Tool(tool),
            decision: Decision::Deny("tool denied by profile".into()),
        });
    }
    for tool in profile.rules.ask_tools {
        rules.push(Rule {
            matcher: RuleMatcher::Tool(tool),
            decision: Decision::Ask("profile requires tool approval".into()),
        });
    }
    if !rules.is_empty() {
        rules.push(Rule {
            matcher: RuleMatcher::Any,
            decision: Decision::Allow,
        });
        config.hooks.push(PolicyHook::new(Arc::new(
            RulePolicy::new(rules).auto_approve(auto_approve.clone()),
        )));
    }
    // The hooks every mode shares. Each declares its stage, so the policies above run first
    // and redaction runs before output is saved, whatever is registered later.
    config.hooks.push(ContinueOnLength::new(profile.continue_on_length));
    // An unset or empty variable holds nothing to redact.
    let hidden_values = options
        .hidden_env
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .filter(|value| !value.is_empty());
    config.hooks.push(
        RedactSecrets::new(api_key.into_iter().chain(hidden_values))
            .with_secret_files(std::iter::once(PathBuf::from(SERVICE_ACCOUNT_TOKEN)).chain(secret_files)),
    );
    if let Some(spill) = spill {
        config.hooks.push(spill);
    }
    // The built-in tools skip repeated names; this guards tools registered here later.
    config.spec.validate().map_err(anyhow::Error::msg)?;
    Ok(PreparedSession {
        config,
        retry: RetryPolicy {
            max_retries: options.transport.max_retries,
            ..RetryPolicy::default()
        },
        store: session,
        auto_approve,
        limits: profile.limits,
        compaction: profile.compaction,
    })
}

impl PreparedSession {
    /// Ask a person before every bash command, except those the profile auto-approves; other
    /// tools follow the profile's rules. Chat and serve both work this way.
    pub(crate) fn require_bash_approval(&mut self, reason: &str) {
        self.config.hooks.push(PolicyHook::new(Arc::new(
            RulePolicy::new(vec![
                Rule {
                    matcher: RuleMatcher::Tool(BASH.into()),
                    decision: Decision::Ask(reason.into()),
                },
                Rule {
                    matcher: RuleMatcher::Any,
                    decision: Decision::Allow,
                },
            ])
            .auto_approve(std::mem::take(&mut self.auto_approve)),
        )));
    }

    /// The conversation, sending events to `sink`: the one place where every mode turns a
    /// prepared session into a running one.
    pub(crate) fn into_session(self, sink: Arc<dyn EventSink>) -> Result<AgentSession> {
        let runner =
            Runner::new(self.config, self.retry, self.limits, self.store, sink).with_compaction(self.compaction)?;
        Ok(AgentSession::from(runner))
    }
}

/// Container settings from the profile; the session file must already be outside `cwd`.
fn container_config(settings: &ContainerSettings, cwd: &Path, label: &str) -> Result<ContainerConfig> {
    let image = settings
        .image
        .clone()
        .context("execution = \"container\" needs container.image (or --container-image)")?;
    let mut config = ContainerConfig {
        label: label.into(),
        ..ContainerConfig::new(image, cwd)
    };
    if let Some(user) = &settings.user {
        config.user.clone_from(user);
    }
    if let Some(memory) = &settings.memory {
        config.memory.clone_from(memory);
    }
    if let Some(limit) = settings.pids_limit {
        config.pids_limit = limit;
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omitted_budgets_and_tools_keep_the_profile_values() {
        let cli = Cli::try_parse_from(["zx-agent", "run", "--model", "m", "go"]).unwrap();
        match cli.command {
            Command::Run(args) => {
                assert_eq!(args.max_turns, None);
                assert_eq!(args.timeout_secs, None);
                assert_eq!(args.continue_on_length, None);
                assert_eq!(args.base.tools, None);
            }
            Command::Chat(_) | Command::Serve(_) => panic!("run command"),
        }
        let cli = Cli::try_parse_from(["zx-agent", "chat", "--model", "m"]).unwrap();
        match cli.command {
            Command::Chat(args) => {
                assert_eq!(args.max_turns, None);
                assert_eq!(args.timeout_secs, None);
                assert_eq!(args.output, OutputMode::Text);
            }
            Command::Run(_) | Command::Serve(_) => panic!("chat command"),
        }
    }

    #[test]
    fn serve_takes_a_configuration_file_and_the_shared_settings() {
        let cli = Cli::try_parse_from(["zx-agent", "serve", "--config", "serve.toml", "--model", "m"]).unwrap();
        match cli.command {
            Command::Serve(args) => {
                assert_eq!(args.config, Path::new("serve.toml"));
                assert_eq!(args.max_turns, None);
                assert_eq!(args.request.session, None);
            }
            Command::Run(_) | Command::Chat(_) => panic!("serve command"),
        }
        assert!(Cli::try_parse_from(["zx-agent", "serve", "--model", "m"]).is_err());
    }

    #[test]
    fn explicit_values_override_even_when_zero_or_empty() {
        let cli =
            Cli::try_parse_from(["zx-agent", "chat", "--model", "m", "--timeout-secs", "0", "--tools", ""]).unwrap();
        match cli.command {
            Command::Chat(args) => {
                assert_eq!(args.timeout_secs, Some(0));
                assert_eq!(args.base.tools, Some(vec![String::new()]));
            }
            Command::Run(_) | Command::Serve(_) => panic!("chat command"),
        }
        let cli = Cli::try_parse_from(["zx-agent", "run", "--model", "m", "--timeout-secs", "900", "go"]).unwrap();
        match cli.command {
            Command::Run(args) => assert_eq!(args.timeout_secs, Some(900)),
            Command::Chat(_) | Command::Serve(_) => panic!("run command"),
        }
    }
}
