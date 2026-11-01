//! Command-line settings and shared runtime assembly for run and chat.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};

use zx_agent::agent::{CommandPolicy, LoopConfig, LoopSpec};
use zx_agent::ai::openai::{DEFAULT_MAX_RESPONSE_BYTES, OpenAiCompatible, OpenAiConfig, SystemUpdates};
use zx_agent::ai::retry::RetryPolicy;
use zx_agent::ai::sse::DEFAULT_MAX_EVENT_BYTES;
use zx_agent::extensions::RedactSecrets;
use zx_agent::policy::PolicyHook;
use zx_agent::presets::troubleshoot;
use zx_agent::prompt::{build_system_prompt, environment_section};
use zx_agent::session::{SessionHeader, SessionStore};
use zx_agent::tools;

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
}

#[derive(Args)]
pub(crate) struct RunArgs {
    /// Task description. Use "-" to read it from stdin.
    pub(crate) prompt: String,

    #[command(flatten)]
    pub(crate) base: BaseArgs,

    /// Maximum model requests for the run, failed attempts included.
    #[arg(long, default_value_t = troubleshoot::MAX_TURNS)]
    pub(crate) max_turns: u32,

    /// Wall-clock limit for the whole run, in seconds. 0 disables it.
    #[arg(long, default_value_t = troubleshoot::TIMEOUT_SECS)]
    pub(crate) timeout_secs: u64,

    #[command(flatten)]
    pub(crate) transport: TransportArgs,

    /// Times to ask the model to continue an answer cut off by the output token limit.
    /// Each continuation counts against --max-turns.
    #[arg(long, default_value_t = troubleshoot::CONTINUE_ON_LENGTH)]
    pub(crate) continue_on_length: u32,

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

    /// Maximum model requests per prompt. Omit to leave it unlimited.
    #[arg(long)]
    pub(crate) max_turns: Option<u32>,

    /// Wall-clock limit per prompt, in seconds. 0 disables it.
    #[arg(long, default_value_t = 0)]
    pub(crate) timeout_secs: u64,

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

    /// Replace the built-in instructions with the contents of this file. The generated
    /// <environment> and <command_policy> sections are still appended.
    #[arg(long)]
    pub(crate) system_prompt_file: Option<PathBuf>,

    /// Working directory for tools. Defaults to the current directory.
    #[arg(long)]
    pub(crate) cwd: Option<PathBuf>,

    /// Enabled tools, comma-separated.
    #[arg(long, value_delimiter = ',', default_value = "bash,read_file")]
    pub(crate) tools: Vec<String>,

    /// Allowlisted command prefix for bash, e.g. --allow-cmd "kubectl get". Repeatable.
    /// When set, bash may only run '|' pipelines of allowlisted commands.
    #[arg(long = "allow-cmd")]
    pub(crate) allow_cmd: Vec<String>,

    /// Extra substring to reject in bash commands. Repeatable.
    #[arg(long = "deny-cmd")]
    pub(crate) deny_cmd: Vec<String>,
}

#[derive(Args)]
pub(crate) struct TransportArgs {
    /// Default bash command timeout, in seconds.
    #[arg(long, default_value_t = 120)]
    pub(crate) tool_timeout_secs: u64,

    /// Longest wait for response headers or the next stream chunk, in seconds.
    #[arg(long, default_value_t = 120)]
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
}

pub(crate) struct SessionOptions<'a> {
    pub(crate) base: &'a BaseArgs,
    pub(crate) transport: &'a TransportArgs,
    pub(crate) request: &'a RequestArgs,
    pub(crate) model: &'a str,
    pub(crate) file: SessionFile<'a>,
    pub(crate) instructions: &'a str,
}

pub(crate) struct PreparedSession {
    pub(crate) config: LoopConfig,
    pub(crate) retry: RetryPolicy,
    pub(crate) store: SessionStore,
    pub(crate) redactor: RedactSecrets,
}

/// Synchronous startup reads and short session appends precede the asynchronous run.
pub(crate) fn prepare(options: SessionOptions<'_>) -> Result<PreparedSession> {
    let cwd = match &options.base.cwd {
        Some(path) => path
            .canonicalize()
            .with_context(|| format!("invalid --cwd {}", path.display()))?,
        None => std::env::current_dir()?,
    };
    let instructions = match &options.base.system_prompt_file {
        Some(path) => std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?,
        None => options.instructions.to_string(),
    };
    // Local servers (vLLM, Ollama, llama.cpp) often need no key, so a missing key is allowed.
    let api_key = std::env::var(&options.base.api_key_env)
        .ok()
        .filter(|key| !key.is_empty());
    // Commands must not inherit the key: `env` would put it into the session file and the context.
    let tools = tools::builtin_tools(
        &options.base.tools,
        Duration::from_secs(options.transport.tool_timeout_secs),
        std::slice::from_ref(&options.base.api_key_env),
    )?;
    let policy = CommandPolicy::new(&options.base.allow_cmd).deny_also(&options.base.deny_cmd);
    // The command policy only concerns bash.
    let command_policy = if tools.iter().any(|tool| tool.name() == "bash") {
        policy.prompt_section().unwrap_or_default()
    } else {
        String::new()
    };
    let system_prompt = build_system_prompt(
        &instructions,
        &[
            ("environment", &environment_section(&cwd)),
            ("command_policy", &command_policy),
        ],
    );

    let header = SessionHeader::new(&cwd, options.model);
    let session = match options.file {
        SessionFile::Memory => SessionStore::in_memory(header),
        SessionFile::Open(path) if path.exists() => SessionStore::open(path)?,
        SessionFile::Open(path) | SessionFile::Create(path) => SessionStore::create(path, header)?,
    };
    if session.recovered_tail_bytes() > 0 {
        eprintln!(
            "[session] recovered incomplete final record; removed {} bytes",
            session.recovered_tail_bytes()
        );
    }

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
    Ok(PreparedSession {
        config,
        retry: RetryPolicy {
            max_retries: options.transport.max_retries,
            ..RetryPolicy::default()
        },
        store: session,
        redactor: RedactSecrets::new(api_key),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_preserves_the_unattended_defaults() {
        let cli = Cli::try_parse_from(["zx-agent", "run", "--model", "m", "go"]).unwrap();
        match cli.command {
            Command::Run(args) => {
                assert_eq!(args.max_turns, 30);
                assert_eq!(args.timeout_secs, 900);
                assert_eq!(args.continue_on_length, 1);
            }
            Command::Chat(_) => panic!("run command"),
        }
    }

    #[test]
    fn chat_has_neutral_prompt_budgets() {
        let cli = Cli::try_parse_from(["zx-agent", "chat", "--model", "m"]).unwrap();
        match cli.command {
            Command::Chat(args) => {
                assert_eq!(args.max_turns, None);
                assert_eq!(args.timeout_secs, 0);
                assert_eq!(args.output, OutputMode::Text);
            }
            Command::Run(_) => panic!("chat command"),
        }
    }
}
