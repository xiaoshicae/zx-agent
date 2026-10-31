use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::json;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

use zx_agent::AgentSession;
use zx_agent::agent::{AgentEvent, CommandPolicy, EventSink, LoopConfig, LoopSpec};
use zx_agent::ai::openai::{DEFAULT_MAX_RESPONSE_BYTES, OpenAiCompatible, OpenAiConfig, SystemUpdates};
use zx_agent::ai::retry::RetryPolicy;
use zx_agent::ai::sse::DEFAULT_MAX_EVENT_BYTES;
use zx_agent::ai::{Message, StopReason};
use zx_agent::extensions::{ContinueOnLength, RedactSecrets};
use zx_agent::policy::PolicyHook;
use zx_agent::presets::troubleshoot;
use zx_agent::prompt::{build_system_prompt, environment_section};
use zx_agent::runner::{Limits, RunOutcome, RunReport};
use zx_agent::session::{SessionHeader, SessionStore};
use zx_agent::tools;

/// Exit codes, so schedulers can react without parsing output.
const EXIT_COMPLETED: u8 = 0;
const EXIT_ERROR: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_MAX_TURNS: u8 = 3;
const EXIT_TIMEOUT: u8 = 4;
const EXIT_ABORTED: u8 = 5;
const EXIT_TRUNCATED: u8 = 6;

#[derive(Parser)]
#[command(name = "zx-agent", version, about = "Minimal general-purpose agent kernel")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run one task to completion without human interaction.
    Run(RunArgs),
}

#[derive(Args)]
struct RunArgs {
    /// Task description. Use "-" to read it from stdin.
    prompt: String,

    /// Model id passed to the provider.
    #[arg(long, env = "ZX_MODEL")]
    model: String,

    /// OpenAI-compatible base URL (without /chat/completions).
    #[arg(long, env = "ZX_BASE_URL", default_value = "https://api.openai.com/v1")]
    base_url: String,

    /// Name of the environment variable holding the API key. The key itself is never
    /// accepted as an argument, so it does not appear in process listings.
    #[arg(long, default_value = "OPENAI_API_KEY")]
    api_key_env: String,

    /// How system prompt changes reach the model when a session is resumed. collapse: fold
    /// every version into the leading system message; works with any server. inline: send
    /// each change where it happened, which keeps the prompt cache and shows the model when
    /// the prompt changed, but needs a server that accepts system messages mid-conversation.
    #[arg(long, value_enum, default_value_t = SystemUpdatesArg::Collapse)]
    system_updates: SystemUpdatesArg,

    /// Replace the built-in instructions with the contents of this file. The generated
    /// <environment> and <command_policy> sections are still appended.
    #[arg(long)]
    system_prompt_file: Option<PathBuf>,

    /// Working directory for tools. Defaults to the current directory.
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Enabled tools, comma-separated.
    #[arg(long, value_delimiter = ',', default_value = "bash,read_file")]
    tools: Vec<String>,

    /// Allowlisted command prefix for bash, e.g. --allow-cmd "kubectl get". Repeatable.
    /// When set, bash may only run '|' pipelines of allowlisted commands.
    #[arg(long = "allow-cmd")]
    allow_cmd: Vec<String>,

    /// Extra substring to reject in bash commands. Repeatable.
    #[arg(long = "deny-cmd")]
    deny_cmd: Vec<String>,

    /// Maximum model requests for the run, failed attempts included.
    #[arg(long, default_value_t = troubleshoot::MAX_TURNS)]
    max_turns: u32,

    /// Wall-clock limit for the whole run, in seconds. 0 disables it.
    #[arg(long, default_value_t = troubleshoot::TIMEOUT_SECS)]
    timeout_secs: u64,

    /// Default bash command timeout, in seconds.
    #[arg(long, default_value_t = 120)]
    tool_timeout_secs: u64,

    /// Longest wait for response headers or the next stream chunk, in seconds.
    #[arg(long, default_value_t = 120)]
    idle_timeout_secs: u64,

    /// Retries for transient provider errors.
    #[arg(long, default_value_t = 3)]
    max_retries: u32,

    /// Times to ask the model to continue an answer cut off by the output token limit.
    /// Each continuation counts against --max-turns.
    #[arg(long, default_value_t = troubleshoot::CONTINUE_ON_LENGTH)]
    continue_on_length: u32,

    /// Output token limit per response. Reasoning models count their reasoning against it,
    /// so a small limit can leave the answer empty.
    #[arg(long)]
    max_tokens: Option<u32>,

    /// Maximum raw streaming response bytes per request, including SSE framing.
    #[arg(long, default_value_t = DEFAULT_MAX_RESPONSE_BYTES)]
    max_response_bytes: usize,

    /// Maximum bytes in one SSE event, including framing.
    #[arg(long, default_value_t = DEFAULT_MAX_EVENT_BYTES)]
    max_sse_event_bytes: usize,

    /// Session file (JSONL). Created if missing; an existing one is continued.
    #[arg(long)]
    session: Option<PathBuf>,

    /// text: progress on stderr, final answer on stdout. jsonl: one event per line on stdout.
    #[arg(long, value_enum, default_value_t = OutputMode::Text)]
    output: OutputMode,

    /// In jsonl mode, also emit streaming message_update events.
    #[arg(long)]
    deltas: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum SystemUpdatesArg {
    Collapse,
    Inline,
}

#[derive(Clone, Copy, ValueEnum)]
enum OutputMode {
    Text,
    Jsonl,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Run(args) => match run(args).await {
            Ok(code) => ExitCode::from(code),
            Err(e) => {
                eprintln!("zx-agent: {e:#}");
                ExitCode::from(EXIT_USAGE)
            }
        },
    }
}

/// Run one task and return its exit code. An `Err` is a configuration problem found
/// before the run started.
async fn run(args: RunArgs) -> Result<u8> {
    let prompt = read_prompt(&args.prompt)?;
    let session = build_session(&args)?;
    let cancel = CancellationToken::new();
    spawn_signal_handler(cancel.clone());
    let report = session.prompt_with_cancel(prompt, &cancel).await?;
    print_report(&report, args.output);
    Ok(exit_code(report.outcome))
}

fn read_prompt(arg: &str) -> Result<String> {
    let prompt = if arg == "-" {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .context("cannot read the prompt from stdin")?;
        text
    } else {
        arg.to_string()
    };
    anyhow::ensure!(!prompt.trim().is_empty(), "the prompt is empty");
    Ok(prompt)
}

/// Assemble the provider, tools, policy, session, and output from the command line.
fn build_session(args: &RunArgs) -> Result<AgentSession> {
    let cwd = match &args.cwd {
        Some(path) => path
            .canonicalize()
            .with_context(|| format!("invalid --cwd {}", path.display()))?,
        None => std::env::current_dir()?,
    };
    let instructions = match &args.system_prompt_file {
        Some(path) => std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?,
        None => troubleshoot::INSTRUCTIONS.to_string(),
    };
    // Local servers (vLLM, Ollama, llama.cpp) often need no key, so a missing key is allowed.
    let api_key = std::env::var(&args.api_key_env).ok().filter(|key| !key.is_empty());
    // Commands must not inherit the key: `env` would put it into the session file and the context.
    let tools = tools::builtin_tools(
        &args.tools,
        Duration::from_secs(args.tool_timeout_secs),
        std::slice::from_ref(&args.api_key_env),
    )?;
    let policy = CommandPolicy::new(&args.allow_cmd).deny_also(&args.deny_cmd);
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

    let header = SessionHeader::new(&cwd, &args.model);
    let session = match &args.session {
        Some(path) if path.exists() => SessionStore::open(path)?,
        Some(path) => SessionStore::create(path, header)?,
        None => SessionStore::in_memory(header),
    };
    if session.recovered_tail_bytes() > 0 {
        eprintln!(
            "[session] recovered incomplete final record; removed {} bytes",
            session.recovered_tail_bytes()
        );
    }

    let provider = OpenAiCompatible::new(OpenAiConfig {
        base_url: args.base_url.clone(),
        // Provider authentication and output filtering each need an owned copy.
        api_key: api_key.clone(),
        idle_timeout: Duration::from_secs(args.idle_timeout_secs),
        max_response_bytes: args.max_response_bytes,
        max_sse_event_bytes: args.max_sse_event_bytes,
        system_updates: match args.system_updates {
            SystemUpdatesArg::Collapse => SystemUpdates::Collapse,
            SystemUpdatesArg::Inline => SystemUpdates::Inline,
        },
        ..OpenAiConfig::default()
    })?;
    let mut config = LoopConfig::new(LoopSpec {
        provider: Arc::new(provider),
        model: args.model.clone(),
        system_prompt,
        tools,
        cwd,
        max_tokens: args.max_tokens,
        sequential_tools: false,
    });
    config.hooks.push(PolicyHook::new(Arc::new(policy)));
    config.hooks.push(ContinueOnLength::new(args.continue_on_length));
    config.hooks.push(RedactSecrets::new(api_key));
    let sink: Arc<dyn EventSink> = match args.output {
        OutputMode::Text => Arc::new(TextProgress),
        OutputMode::Jsonl => Arc::new(JsonlEvents { deltas: args.deltas }),
    };
    let retry = RetryPolicy {
        max_retries: args.max_retries,
        ..RetryPolicy::default()
    };
    let limits = Limits {
        max_turns: Some(args.max_turns),
        timeout: (args.timeout_secs > 0).then(|| Duration::from_secs(args.timeout_secs)),
    };
    Ok(AgentSession::new(config, retry, limits, session, sink))
}

fn print_report(report: &RunReport, output: OutputMode) {
    match output {
        OutputMode::Text => {
            if !report.final_text.is_empty() {
                println!("{}", report.final_text);
            }
            let usage = &report.usage;
            let error = report
                .error
                .as_deref()
                .map(|e| format!(" error={e}"))
                .unwrap_or_default();
            let session = report
                .session_file
                .as_deref()
                .map(|file| format!(" session={file}"))
                .unwrap_or_default();
            eprintln!(
                "[zx-agent] outcome={:?} turns={} retries={} tokens={} (in {} / out {}){error}{session}",
                report.outcome, report.turns, report.retries, usage.total_tokens, usage.input, usage.output,
            );
        }
        OutputMode::Jsonl => write_line(&json!({ "type": "run_end", "report": report })),
    }
}

fn exit_code(outcome: RunOutcome) -> u8 {
    match outcome {
        RunOutcome::Completed => EXIT_COMPLETED,
        RunOutcome::Error => EXIT_ERROR,
        RunOutcome::MaxTurns => EXIT_MAX_TURNS,
        RunOutcome::Timeout => EXIT_TIMEOUT,
        RunOutcome::Aborted => EXIT_ABORTED,
        RunOutcome::Truncated => EXIT_TRUNCATED,
    }
}

/// SIGINT and SIGTERM cancel the run; the report is still written.
///
/// The listeners are registered before this returns, so a signal that arrives once the run
/// has started always cancels it instead of killing the process. The waiting task is detached
/// on purpose: it lives until a signal arrives or the process exits.
fn spawn_signal_handler(cancel: CancellationToken) {
    match shutdown_signal() {
        Ok(signal) => {
            tokio::spawn(async move {
                signal.await;
                cancel.cancel();
            });
        }
        // Without signal handling the run is still bounded by its own limits, so it continues.
        Err(e) => eprintln!("zx-agent: cannot listen for shutdown signals: {e}"),
    }
}

/// Register SIGINT and SIGTERM listeners; the future completes at the first signal.
#[cfg(unix)]
fn shutdown_signal() -> std::io::Result<impl std::future::Future<Output = ()>> {
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    Ok(async move {
        // `recv` yields `None` only when the runtime shuts down, which ends the process anyway.
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
        }
    })
}

/// Register a Ctrl+C listener; the future completes at the first Ctrl+C.
#[cfg(not(unix))]
fn shutdown_signal() -> std::io::Result<impl std::future::Future<Output = ()>> {
    let mut ctrl_c = tokio::signal::windows::ctrl_c()?;
    Ok(async move {
        // `recv` yields `None` only when the runtime shuts down, which ends the process anyway.
        ctrl_c.recv().await;
    })
}

/// Print one JSON line on stdout.
fn write_line(value: &impl serde::Serialize) {
    // Events and reports hold only strings, numbers, and `serde_json::Value`, which always serialize.
    let Ok(line) = serde_json::to_string(value) else {
        return;
    };
    let mut out = std::io::stdout().lock();
    // A closed stdout (for example a consumer that exited) leaves nobody to report to; the run
    // continues and the exit code still tells the scheduler how it ended.
    let _ = writeln!(out, "{line}").and_then(|()| out.flush());
}

/// Writes every event as one JSON line on stdout.
struct JsonlEvents {
    deltas: bool,
}

impl EventSink for JsonlEvents {
    fn emit(&self, event: AgentEvent) {
        if !self.deltas && matches!(event, AgentEvent::MessageUpdate { .. }) {
            return;
        }
        write_line(&event);
    }
}

/// Short progress lines on stderr.
struct TextProgress;

impl EventSink for TextProgress {
    fn emit(&self, event: AgentEvent) {
        match event {
            AgentEvent::ToolExecutionStart { tool_name, args, .. } => {
                eprintln!("[tool] {tool_name} {}", clip(&args.to_string(), 300));
            }
            AgentEvent::ToolExecutionEnd {
                tool_name,
                is_error,
                duration_ms,
                ..
            } => {
                let status = if is_error { "error" } else { "ok" };
                let duration = duration_ms.map(|ms| format!(" {ms}ms")).unwrap_or_default();
                eprintln!("[tool] {tool_name} -> {status}{duration}");
            }
            AgentEvent::MessageEnd {
                message: Message::Assistant(message),
            } => {
                if let Some(error) = &message.error_message {
                    eprintln!("[model] {:?}: {error}", message.stop_reason);
                } else if message.stop_reason == StopReason::Length {
                    eprintln!("[model] Length: the response hit the output token limit");
                }
            }
            AgentEvent::RetryScheduled {
                attempt,
                max_retries,
                delay_ms,
                error,
            } => {
                eprintln!("[retry] {attempt}/{max_retries} in {delay_ms}ms: {error}");
            }
            // Progress output only reports tool activity, model errors, and retries.
            _ => {}
        }
    }
}

fn clip(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((index, _)) => format!("{}...", &text[..index]),
        None => text.to_string(),
    }
}
