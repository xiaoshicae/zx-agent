//! The command line: the subcommands, their arguments and the value parsers.
//!
//! - `shared`: what every session of a process shares, read or connected once
//! - `prepare`: the assembly of one session, shared by run, chat, serve and acp
//! - `execution`: where a session's commands run: this machine or a Docker container

mod execution;
mod prepare;
mod shared;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};

use zx_agent::agent::QueueMode;
use zx_agent::ai::Reasoning;
use zx_agent::ai::openai::{DEFAULT_MAX_RESPONSE_BYTES, ReasoningFormat};
use zx_agent::ai::sse::DEFAULT_MAX_EVENT_BYTES;

pub(crate) use prepare::{PreparedSession, SessionOptions, prepare};
pub(crate) use shared::{ModeSettings, Shared};

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
    /// List the session files chat keeps for a working directory, most recent first.
    Sessions(SessionsArgs),
    /// Serve the Agent Client Protocol over stdio for an editor or a terminal client.
    Acp(AcpArgs),
}

#[derive(Args)]
pub(crate) struct RunArgs {
    /// Task description. Use "-" to read it from stdin. Omitted with --continue.
    #[arg(required_unless_present = "continue_session", conflicts_with = "continue_session")]
    pub(crate) prompt: Option<String>,

    /// Continue the conversation in --session without new input: answer the input or tool
    /// results it ends with, for example after a crash, or answer again after a failed or
    /// cancelled answer. A session that ends with a finished answer exits with code 1.
    #[arg(long = "continue", requires = "session")]
    pub(crate) continue_session: bool,

    /// Exit with code 9 instead of 0 when the run completed but a policy or an approval
    /// denied at least one tool call, which the model may have worked around.
    #[arg(long)]
    pub(crate) fail_on_denied: bool,

    #[command(flatten)]
    pub(crate) base: BaseArgs,

    #[command(flatten)]
    pub(crate) budget: BudgetArgs,

    #[command(flatten)]
    pub(crate) transport: TransportArgs,

    /// Times to ask the model to continue an answer cut off by the output token limit.
    /// Each continuation counts against --max-model-requests. Defaults to the selected profile.
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
#[expect(clippy::struct_excessive_bools, reason = "command-line switches")]
pub(crate) struct ChatArgs {
    #[command(flatten)]
    pub(crate) base: BaseArgs,

    #[command(flatten)]
    pub(crate) budget: BudgetArgs,

    /// How long an approval request waits for a menu choice, in seconds. An unanswered
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

    /// Show detailed execution events on stderr in text mode.
    #[arg(long)]
    pub(crate) debug: bool,

    /// Stream the model's reasoning, dimmed, on stderr in text mode.
    #[arg(long)]
    pub(crate) show_thinking: bool,

    /// In jsonl mode, also emit streaming message_update events.
    #[arg(long)]
    pub(crate) deltas: bool,

    /// Color the text output on stderr. auto: on an interactive terminal unless NO_COLOR is
    /// set or TERM is dumb. always: also when piped, e.g. into `less -R`.
    #[arg(long, value_enum, default_value_t = ColorArg::Auto)]
    pub(crate) color: ColorArg,

    /// Directory for new session files and for --resume, /sessions and /resume. Defaults to
    /// ~/.zx-agent/sessions/--<cwd with / as ->--, one directory per working directory.
    #[arg(long, conflicts_with = "session")]
    pub(crate) session_dir: Option<PathBuf>,

    /// Keep the conversation in memory only: no session file is written.
    #[arg(long, conflicts_with_all = ["session", "session_dir", "resume"])]
    pub(crate) no_session: bool,

    /// Resume a session from the session directory: the most recent one started in this
    /// working directory, or the one whose ID starts with the value (see `zx-agent sessions`).
    #[arg(long, value_name = "ID", num_args = 0..=1, conflicts_with = "session")]
    pub(crate) resume: Option<Option<String>>,

    /// Start with a copy of another session: a session file, or the session in the session
    /// directory whose ID starts with the value. The copy is a new file in the session
    /// directory; the original is not changed.
    #[arg(long, value_name = "ID|PATH", conflicts_with_all = ["session", "no_session", "resume"])]
    pub(crate) fork: Option<String>,

    /// How input typed while a task runs is taken: `all` (default) sends every waiting
    /// message before the next model request; `one-at-a-time` sends the oldest only, so
    /// each gets an answer of its own. /queue changes it while chatting.
    #[arg(long, value_name = "MODE", value_parser = parse_queue_mode)]
    pub(crate) queue_mode: Option<QueueMode>,
}

fn parse_queue_mode(text: &str) -> Result<QueueMode, String> {
    text.parse()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum ColorArg {
    Auto,
    Always,
    Never,
}

#[derive(Args)]
pub(crate) struct AcpArgs {
    #[command(flatten)]
    pub(crate) base: BaseArgs,

    #[command(flatten)]
    pub(crate) budget: BudgetArgs,

    /// How long a permission request waits for the client's answer, in seconds. An
    /// unanswered request is denied, and the turn goes on without the call.
    #[arg(long, default_value_t = 3600, value_parser = clap::value_parser!(u64).range(1..))]
    pub(crate) approval_timeout_secs: u64,

    /// Directory for session files, one per session, where `session/load` finds them again.
    /// Defaults to ~/.zx-agent/sessions/--<cwd with / as ->--, one directory per working
    /// directory, shared with chat.
    #[arg(long, conflicts_with = "session")]
    pub(crate) session_dir: Option<PathBuf>,

    /// Keep conversations in memory only: no session file is written, and `session/load` is
    /// not offered.
    #[arg(long, conflicts_with_all = ["session", "session_dir"])]
    pub(crate) no_session: bool,

    #[command(flatten)]
    pub(crate) transport: TransportArgs,

    #[command(flatten)]
    pub(crate) request: RequestArgs,
}

#[derive(Args)]
pub(crate) struct ServeArgs {
    /// Serve configuration (TOML): platforms, who may use the bot, limits and the session
    /// directory. See README, section "IM 服务".
    #[arg(long)]
    pub(crate) config: PathBuf,

    #[command(flatten)]
    pub(crate) base: BaseArgs,

    #[command(flatten)]
    pub(crate) budget: BudgetArgs,

    #[command(flatten)]
    pub(crate) transport: TransportArgs,

    #[command(flatten)]
    pub(crate) request: RequestArgs,
}

#[derive(Args)]
pub(crate) struct SessionsArgs {
    /// Directory of session files. Defaults to the one chat uses for --cwd:
    /// ~/.zx-agent/sessions/--<cwd with / as ->--.
    #[arg(long)]
    pub(crate) session_dir: Option<PathBuf>,

    /// Working directory whose default session directory is listed.
    #[arg(long)]
    pub(crate) cwd: Option<PathBuf>,
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

    /// Tokens the model's context holds, for the profile's [[models]] entry of --model or in
    /// place of one. With automatic summaries on, a summary also runs when the estimated
    /// context reaches the window minus compaction.reserveTokens.
    #[arg(long, value_name = "TOKENS", value_parser = clap::value_parser!(u64).range(1..))]
    pub(crate) context_window: Option<u64>,

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

    /// Allowlisted command prefix for bash, e.g. --allow-cmd "df". Repeatable.
    /// When set, bash may only run '|' pipelines of allowlisted commands.
    #[arg(long = "allow-cmd")]
    pub(crate) allow_cmd: Vec<String>,

    /// Extra substring to reject in bash commands. Repeatable.
    #[arg(long = "deny-cmd")]
    pub(crate) deny_cmd: Vec<String>,

    /// Command prefix for bash that runs without asking where bash needs approval, as in
    /// chat, e.g. --auto-approve-cmd "df". Repeatable; replaces the profile's list. Commands
    /// with quotes, `$`, other shell syntax or a URL still ask, as do those the profile's
    /// autoApproveExcept and autoApproveOptions exclude.
    #[arg(long = "auto-approve-cmd")]
    pub(crate) auto_approve_cmd: Vec<String>,

    /// File holding a secret to redact from tool output, such as a token mounted for the
    /// agent, or a directory of such files (names starting with a dot are skipped). Each path
    /// must exist at startup, and each file must be a regular file of at most 64 KiB. Read
    /// again before every redaction. Repeatable, and added to the files in the
    /// comma-separated ZX_SECRET_FILES.
    #[arg(long = "secret-file", value_delimiter = ',')]
    pub(crate) secret_files: Vec<PathBuf>,

    /// Embedded profile name (unattended, troubleshoot, assistant) or path to a TOML file.
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

    /// Where bash and read_file run. local: this machine. container: a Docker container that
    /// is the sandbox: by default the working directory and the container's files are
    /// writable and it has network access; the profile's [container] section can make it
    /// stricter. Defaults to the selected profile.
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

    /// A header to send with every model request, as NAME: VALUE, such as a gateway's routing
    /// header. Repeatable. Authorization is refused: the key comes from --api-key-env. The
    /// value is visible in process listings and is not recorded in the session.
    #[arg(long = "header", value_name = "NAME: VALUE", value_parser = parse_header)]
    pub(crate) headers: Vec<(String, String)>,
}

fn parse_header(text: &str) -> Result<(String, String), String> {
    let (name, value) = text
        .split_once(':')
        .ok_or_else(|| format!("expected NAME: VALUE, got {text:?}"))?;
    let name = name.trim();
    if name.is_empty() {
        return Err(format!("expected NAME: VALUE, got {text:?}"));
    }
    Ok((name.to_string(), value.trim().to_string()))
}

/// Limits per prompt; `None` keeps the selected profile's value. A run is one prompt.
#[derive(Args)]
pub(crate) struct BudgetArgs {
    /// Maximum model requests per prompt, failed attempts included. Defaults to the selected
    /// profile.
    #[arg(long)]
    pub(crate) max_model_requests: Option<u32>,

    /// Wall-clock limit per prompt, in seconds. 0 disables it. Defaults to the selected
    /// profile.
    #[arg(long)]
    pub(crate) timeout_secs: Option<u64>,

    /// Tokens per prompt: input plus output of its model requests, as the provider reports
    /// them. Checked before each request, so the last one may go over; a run then exits 7.
    /// 0 disables it. Defaults to the selected profile.
    #[arg(long)]
    pub(crate) token_budget: Option<u64>,

    /// Spending per prompt, in the unit of the prices in the profile's [[models]] (any
    /// currency, as long as every price uses it): the cost of its model requests by those
    /// prices. Checked before each request, so the last one may go over; a run then exits 8.
    /// The model must have prices. 0 disables it. Defaults to the selected profile.
    #[arg(long, value_parser = parse_cost_budget)]
    pub(crate) cost_budget: Option<f64>,
}

/// A cost budget on the command line: zero or a positive, finite amount.
fn parse_cost_budget(text: &str) -> Result<f64, String> {
    match text.trim().parse::<f64>() {
        Ok(amount) if amount.is_finite() && amount >= 0.0 => Ok(amount),
        _ => Err(format!("expected zero (none) or a positive amount, not {text:?}")),
    }
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

    /// A field to add to every request body, as KEY=JSON, for settings that some servers
    /// offer, such as thinking='{"type":"disabled"}' on DeepSeek. Repeatable. Fields that
    /// zx-agent sets itself, such as model and max_tokens, are refused. The session's request
    /// audit records them.
    #[arg(long = "request-field", value_name = "KEY=JSON")]
    pub(crate) request_fields: Vec<String>,

    /// How hard a reasoning model thinks: off, minimal, low, medium, high or xhigh. Unset,
    /// requests carry no reasoning setting and the server decides. chat can change it with
    /// /reasoning. The session's request audit records it.
    #[arg(long, value_parser = parse_reasoning)]
    pub(crate) reasoning: Option<Reasoning>,

    /// How --reasoning is written in the request: effort (reasoning_effort, the OpenAI field;
    /// off sends nothing), thinking (a thinking object, enabled or disabled, with
    /// reasoning_effort) or reasoning-object (reasoning.effort). Check which fields the server
    /// documents. The session's request audit records it.
    #[arg(long, value_parser = parse_reasoning_format, default_value = "effort")]
    pub(crate) reasoning_format: ReasoningFormat,

    /// Once one earlier answer carries reasoning_content, send the others with an empty one,
    /// for thinking-mode servers that reject a history in which only some answers carry it.
    /// The session's request audit records the choice.
    #[arg(long)]
    pub(crate) fill_missing_reasoning: bool,

    /// Session file (JSONL). Created if missing; an existing one is continued.
    #[arg(long)]
    pub(crate) session: Option<PathBuf>,

    /// Send the session ID as the request's prompt_cache_key, so that the requests of one
    /// conversation share the server's prompt cache (OpenAI takes the field; servers that
    /// reject unknown fields must not get it). The request audit records the key.
    #[arg(long)]
    pub(crate) prompt_cache_key: bool,
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

fn parse_reasoning(text: &str) -> Result<Reasoning, String> {
    text.parse()
}

fn parse_reasoning_format(text: &str) -> Result<ReasoningFormat, String> {
    text.parse()
}

/// The working directory as `prepare` resolves it, which is what session headers record.
pub(crate) fn resolve_cwd(cwd: Option<&Path>) -> Result<PathBuf> {
    let cwd = match cwd {
        Some(path) => path
            .canonicalize()
            .with_context(|| format!("invalid --cwd {}", path.display()))?,
        None => std::env::current_dir().context("cannot find the current directory")?,
    };
    anyhow::ensure!(cwd.is_dir(), "--cwd is not a directory: {}", cwd.display());
    Ok(cwd)
}

/// The value of the environment variable `name`, when it is set and not empty.
pub(crate) fn env_value(name: &str) -> Option<String> {
    // A value that is not UTF-8 holds no usable key, token or path: it counts as unset.
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_request_limit_is_available_in_each_mode() {
        let flag = "--max-model-requests";
        let cli = Cli::try_parse_from(["zx-agent", "run", "--model", "m", flag, "3", "go"]).unwrap();
        match cli.command {
            Command::Run(args) => assert_eq!(args.budget.max_model_requests, Some(3)),
            _ => panic!("run command"),
        }
        let cli = Cli::try_parse_from(["zx-agent", "chat", "--model", "m", flag, "3"]).unwrap();
        match cli.command {
            Command::Chat(args) => assert_eq!(args.budget.max_model_requests, Some(3)),
            _ => panic!("chat command"),
        }
        let cli =
            Cli::try_parse_from(["zx-agent", "serve", "--config", "serve.toml", "--model", "m", flag, "3"]).unwrap();
        match cli.command {
            Command::Serve(args) => assert_eq!(args.budget.max_model_requests, Some(3)),
            _ => panic!("serve command"),
        }
        assert!(Cli::try_parse_from(["zx-agent", "run", "--model", "m", "--max-turns", "3", "go"]).is_err());
    }

    #[test]
    fn omitted_budgets_and_tools_keep_the_profile_values() {
        let cli = Cli::try_parse_from(["zx-agent", "run", "--model", "m", "go"]).unwrap();
        match cli.command {
            Command::Run(args) => {
                assert_eq!(args.budget.max_model_requests, None);
                assert_eq!(args.budget.timeout_secs, None);
                assert_eq!(args.continue_on_length, None);
                assert_eq!(args.base.tools, None);
            }
            Command::Chat(_) | Command::Serve(_) | Command::Sessions(_) | Command::Acp(_) => panic!("run command"),
        }
        let cli = Cli::try_parse_from(["zx-agent", "chat", "--model", "m"]).unwrap();
        match cli.command {
            Command::Chat(args) => {
                assert_eq!(args.budget.max_model_requests, None);
                assert_eq!(args.budget.timeout_secs, None);
                assert_eq!(args.output, OutputMode::Text);
            }
            Command::Run(_) | Command::Serve(_) | Command::Sessions(_) | Command::Acp(_) => panic!("chat command"),
        }
    }

    #[test]
    fn serve_takes_a_configuration_file_and_the_shared_settings() {
        let cli = Cli::try_parse_from(["zx-agent", "serve", "--config", "serve.toml", "--model", "m"]).unwrap();
        match cli.command {
            Command::Serve(args) => {
                assert_eq!(args.config, Path::new("serve.toml"));
                assert_eq!(args.budget.max_model_requests, None);
                assert_eq!(args.request.session, None);
            }
            Command::Run(_) | Command::Chat(_) | Command::Sessions(_) | Command::Acp(_) => panic!("serve command"),
        }
        assert!(Cli::try_parse_from(["zx-agent", "serve", "--model", "m"]).is_err());
    }

    #[test]
    fn chat_resumes_the_latest_session_or_one_by_id_and_refuses_conflicting_storage() {
        let resume = |args: &[&str]| {
            let cli = Cli::try_parse_from(["zx-agent", "chat", "--model", "m"].iter().chain(args)).unwrap();
            match cli.command {
                Command::Chat(args) => (args.resume, args.session_dir, args.no_session),
                Command::Run(_) | Command::Serve(_) | Command::Sessions(_) | Command::Acp(_) => {
                    panic!("chat command")
                }
            }
        };
        assert_eq!(resume(&[]), (None, None, false));
        assert_eq!(resume(&["--resume"]), (Some(None), None, false));
        assert_eq!(
            resume(&["--resume", "0b7c", "--session-dir", "d"]),
            (Some(Some("0b7c".into())), Some(PathBuf::from("d")), false)
        );
        assert_eq!(resume(&["--no-session"]), (None, None, true));
        for conflict in [
            &["--session", "s.jsonl", "--resume"][..],
            &["--session", "s.jsonl", "--session-dir", "d"],
            &["--no-session", "--session-dir", "d"],
            &["--no-session", "--resume"],
            &["--no-session", "--session", "s.jsonl"],
        ] {
            let args = ["zx-agent", "chat", "--model", "m"].iter().chain(conflict);
            assert!(Cli::try_parse_from(args).is_err(), "{conflict:?}");
        }
        // run keeps its storage: a file only when --session names one.
        assert!(Cli::try_parse_from(["zx-agent", "run", "--model", "m", "--resume", "go"]).is_err());
        assert!(Cli::try_parse_from(["zx-agent", "run", "--model", "m", "--fork", "a", "go"]).is_err());
        for conflict in [
            &["--fork", "a", "--resume"][..],
            &["--fork", "a", "--session", "s.jsonl"],
            &["--fork", "a", "--no-session"],
        ] {
            let args = ["zx-agent", "chat", "--model", "m"].iter().chain(conflict);
            assert!(Cli::try_parse_from(args).is_err(), "{conflict:?}");
        }
        let cli = Cli::try_parse_from([
            "zx-agent",
            "chat",
            "--model",
            "m",
            "--fork",
            "0b7c",
            "--session-dir",
            "d",
        ])
        .unwrap();
        match cli.command {
            Command::Chat(args) => assert_eq!(args.fork.as_deref(), Some("0b7c")),
            Command::Run(_) | Command::Serve(_) | Command::Sessions(_) | Command::Acp(_) => panic!("chat command"),
        }
    }

    #[test]
    fn explicit_values_override_even_when_zero_or_empty() {
        let cli =
            Cli::try_parse_from(["zx-agent", "chat", "--model", "m", "--timeout-secs", "0", "--tools", ""]).unwrap();
        match cli.command {
            Command::Chat(args) => {
                assert_eq!(args.budget.timeout_secs, Some(0));
                assert_eq!(args.base.tools, Some(vec![String::new()]));
            }
            Command::Run(_) | Command::Serve(_) | Command::Sessions(_) | Command::Acp(_) => panic!("chat command"),
        }
        let cli = Cli::try_parse_from(["zx-agent", "run", "--model", "m", "--timeout-secs", "900", "go"]).unwrap();
        match cli.command {
            Command::Run(args) => assert_eq!(args.budget.timeout_secs, Some(900)),
            Command::Chat(_) | Command::Serve(_) | Command::Sessions(_) | Command::Acp(_) => panic!("run command"),
        }
    }
}
