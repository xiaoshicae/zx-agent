//! The command line: the subcommands and their arguments. Everything that describes the
//! environment rather than one invocation is in config.toml (`zx_app::config::Config`);
//! what one invocation says over it is [`Overrides`].

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use zx_app::Overrides;

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
    /// List the sessions kept for a working directory, chat's and acp's, most recent first.
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

    /// Session file (JSONL). Created if missing; an existing one is continued. Without it
    /// the run is kept in memory only.
    #[arg(long)]
    pub(crate) session: Option<PathBuf>,

    #[command(flatten)]
    pub(crate) base: BaseArgs,

    #[command(flatten)]
    pub(crate) budget: BudgetArgs,

    /// text: progress on stderr, final answer on stdout. jsonl: one event per line on stdout.
    #[arg(long, value_enum, default_value_t = OutputMode::Text)]
    pub(crate) output: OutputMode,

    /// In jsonl mode, also emit streaming message_update events.
    #[arg(long)]
    pub(crate) deltas: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum OutputMode {
    Text,
    Jsonl,
}

#[derive(Args)]
pub(crate) struct ChatArgs {
    #[command(flatten)]
    pub(crate) base: BaseArgs,

    #[command(flatten)]
    pub(crate) budget: BudgetArgs,

    /// text: stream replies on stdout. jsonl: emit events and per-prompt reports.
    #[arg(long, value_enum, default_value_t = OutputMode::Text)]
    pub(crate) output: OutputMode,

    /// In jsonl mode, also emit streaming message_update events.
    #[arg(long)]
    pub(crate) deltas: bool,

    /// Color the text output on stderr. auto: on an interactive terminal unless NO_COLOR is
    /// set or TERM is dumb. always: also when piped, e.g. into `less -R`.
    #[arg(long, value_enum, default_value_t = ColorArg::Auto)]
    pub(crate) color: ColorArg,

    /// Session file (JSONL). Created if missing; an existing one is continued; /new creates
    /// another beside it. Without it, each conversation gets a file in the session directory.
    #[arg(long)]
    pub(crate) session: Option<PathBuf>,

    /// Keep the conversation in memory only: no session file is written.
    #[arg(long, conflicts_with_all = ["session", "resume"])]
    pub(crate) no_session: bool,

    /// Resume a session from the session directory, sessions/--<cwd with / as ->-- in the
    /// zx-agent home (ZX_AGENT_HOME, else ~/.zx-agent): the most recent one started in this
    /// working directory, or the one whose ID starts with the value (see `zx-agent sessions`).
    #[arg(long, value_name = "ID", num_args = 0..=1, conflicts_with = "session")]
    pub(crate) resume: Option<Option<String>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum ColorArg {
    Auto,
    Always,
    Never,
}

/// What `--cwd` means to `acp`, where the client names the directory of every session it
/// creates.
const ACP_CWD_HELP: &str = "Directory whose sessions session/list shows when the client names none. Defaults to \
                            the current directory. A session works in the directory its client creates it for";

#[derive(Args)]
#[command(mut_arg("cwd", |arg| arg.help(ACP_CWD_HELP)))]
pub(crate) struct AcpArgs {
    #[command(flatten)]
    pub(crate) base: BaseArgs,

    #[command(flatten)]
    pub(crate) budget: BudgetArgs,

    /// Keep conversations in memory only: no session file is written, and `session/load` is
    /// not offered. Otherwise each session has a file in sessions/--<cwd with / as ->-- of
    /// the zx-agent home, the directory chat uses.
    #[arg(long)]
    pub(crate) no_session: bool,
}

#[derive(Args)]
pub(crate) struct SessionsArgs {
    /// Working directory whose session directory is listed: sessions/--<cwd with / as ->--
    /// in the zx-agent home (ZX_AGENT_HOME, else ~/.zx-agent). Defaults to the current one.
    #[arg(long)]
    pub(crate) cwd: Option<PathBuf>,
}

// zx-im has these options and those of `BudgetArgs`, with the same help: zx-app does not use
// clap, and the two programs share no code.
#[derive(Args)]
pub(crate) struct BaseArgs {
    /// Model id passed to the provider. Defaults to ZX_MODEL, else model in config.toml.
    #[arg(long)]
    pub(crate) model: Option<String>,

    /// Working directory for tools. Defaults to the current directory.
    #[arg(long)]
    pub(crate) cwd: Option<PathBuf>,

    /// Enabled tools, comma-separated. Defaults to the agent's; explicit values replace them.
    #[arg(long, value_delimiter = ',')]
    pub(crate) tools: Option<Vec<String>>,
}

/// RunLimits per prompt; `None` keeps the agent's value. A run is one prompt.
#[derive(Args)]
pub(crate) struct BudgetArgs {
    /// Maximum model requests per prompt, failed attempts included. Defaults to the agent's.
    #[arg(long)]
    pub(crate) max_model_requests: Option<u32>,

    /// Wall-clock limit per prompt, in seconds. 0 disables it. Defaults to the agent's.
    #[arg(long)]
    pub(crate) timeout_secs: Option<u64>,

    /// Tokens per prompt: input plus output of its model requests, as the provider reports
    /// them. Checked before each request, so the last one may go over; a run then exits 7.
    /// 0 disables it. Defaults to the agent's.
    #[arg(long)]
    pub(crate) token_budget: Option<u64>,

    /// Spending per prompt, in the unit of the prices in [[models]] of config.toml (any
    /// currency, as long as every price uses it): the cost of its model requests by those
    /// prices. Checked before each request, so the last one may go over; a run then exits 8.
    /// The model must have prices. 0 disables it. Defaults to the agent's.
    #[arg(long)]
    pub(crate) cost_budget: Option<f64>,
}

/// What `base` and `budget` say over the agent and config.toml, as the application takes it
/// and checks it.
pub(crate) fn overrides(base: BaseArgs, budget: &BudgetArgs) -> Overrides {
    Overrides {
        model: base.model,
        cwd: base.cwd,
        tools: base.tools,
        max_model_requests: budget.max_model_requests,
        timeout_secs: budget.timeout_secs,
        token_budget: budget.token_budget,
        cost_budget: budget.cost_budget,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_request_limit_is_available_in_each_subcommand_that_runs() {
        let flag = "--max-model-requests";
        for subcommand in [&["run", "go"][..], &["chat"], &["acp"]] {
            let args = ["zx-agent", subcommand[0], "--model", "m", flag, "3"];
            let cli = Cli::try_parse_from(args.into_iter().chain(subcommand[1..].iter().copied())).unwrap();
            let budget = match cli.command {
                Command::Run(args) => args.budget,
                Command::Chat(args) => args.budget,
                Command::Acp(args) => args.budget,
                Command::Sessions(_) => panic!("{subcommand:?} parsed as sessions"),
            };
            assert_eq!(budget.max_model_requests, Some(3), "{subcommand:?}");
        }
        assert!(Cli::try_parse_from(["zx-agent", "run", "--model", "m", "--max-turns", "3", "go"]).is_err());
    }

    #[test]
    fn omitted_budgets_and_tools_keep_the_agent_values() {
        let cli = Cli::try_parse_from(["zx-agent", "run", "--model", "m", "go"]).unwrap();
        match cli.command {
            Command::Run(args) => {
                assert_eq!(args.budget.max_model_requests, None);
                assert_eq!(args.budget.timeout_secs, None);
                assert_eq!(args.base.tools, None);
            }
            Command::Chat(_) | Command::Sessions(_) | Command::Acp(_) => panic!("run command"),
        }
        let cli = Cli::try_parse_from(["zx-agent", "chat", "--model", "m"]).unwrap();
        match cli.command {
            Command::Chat(args) => {
                assert_eq!(args.budget.max_model_requests, None);
                assert_eq!(args.budget.timeout_secs, None);
                assert_eq!(args.output, OutputMode::Text);
            }
            Command::Run(_) | Command::Sessions(_) | Command::Acp(_) => panic!("chat command"),
        }
    }

    #[test]
    fn acp_takes_no_session_file() {
        // Each session of acp has a file of its own.
        assert!(Cli::try_parse_from(["zx-agent", "acp", "--session", "s.jsonl"]).is_err());
    }

    #[test]
    fn acp_describes_cwd_as_the_directory_of_a_listing_not_of_tools() {
        let cwd_help = |name: &str| {
            let command = <Cli as clap::CommandFactory>::command();
            let subcommand = command.find_subcommand(name).unwrap();
            let cwd = subcommand.get_arguments().find(|arg| arg.get_id() == "cwd").unwrap();
            cwd.get_help().unwrap().to_string()
        };
        // The client names the directory of each session it creates.
        assert!(cwd_help("acp").contains("session/list"), "{}", cwd_help("acp"));
        for subcommand in ["run", "chat"] {
            assert_eq!(
                cwd_help(subcommand),
                "Working directory for tools. Defaults to the current directory"
            );
        }
    }

    #[test]
    fn chat_resumes_the_latest_session_or_one_by_id_and_refuses_conflicting_storage() {
        let resume = |args: &[&str]| {
            let cli = Cli::try_parse_from(["zx-agent", "chat", "--model", "m"].iter().chain(args)).unwrap();
            match cli.command {
                Command::Chat(args) => (args.resume, args.no_session),
                Command::Run(_) | Command::Sessions(_) | Command::Acp(_) => {
                    panic!("chat command")
                }
            }
        };
        assert_eq!(resume(&[]), (None, false));
        assert_eq!(resume(&["--resume"]), (Some(None), false));
        assert_eq!(resume(&["--resume", "0b7c"]), (Some(Some("0b7c".into())), false));
        assert_eq!(resume(&["--no-session"]), (None, true));
        for conflict in [
            &["--session", "s.jsonl", "--resume"][..],
            &["--no-session", "--resume"],
            &["--no-session", "--session", "s.jsonl"],
        ] {
            let args = ["zx-agent", "chat", "--model", "m"].iter().chain(conflict);
            assert!(Cli::try_parse_from(args).is_err(), "{conflict:?}");
        }
        // run keeps its storage: a file only when --session names one.
        assert!(Cli::try_parse_from(["zx-agent", "run", "--model", "m", "--resume", "go"]).is_err());
        // chat starts from no copy of another session.
        assert!(Cli::try_parse_from(["zx-agent", "chat", "--model", "m", "--fork", "0b7c"]).is_err());
    }

    #[test]
    fn settings_of_config_toml_are_not_command_line_options() {
        for flag in [
            "--base-url",
            "--api-key-env",
            "--system-updates",
            "--reasoning",
            "--reasoning-format",
            "--fill-missing-reasoning",
            "--prompt-cache-key",
            "--request-field",
            "--header",
            "--idle-timeout-secs",
            "--max-retries",
            "--max-tokens",
            "--tool-timeout-secs",
            "--secret-file",
            "--approval-timeout-secs",
            "--session-dir",
            "--queue-mode",
            "--profile",
            "--agent",
        ] {
            let args = ["zx-agent", "chat", "--model", "m", flag, "x"];
            assert!(Cli::try_parse_from(args).is_err(), "{flag}");
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
            Command::Run(_) | Command::Sessions(_) | Command::Acp(_) => panic!("chat command"),
        }
        let cli = Cli::try_parse_from(["zx-agent", "run", "--model", "m", "--timeout-secs", "900", "go"]).unwrap();
        match cli.command {
            Command::Run(args) => assert_eq!(args.budget.timeout_secs, Some(900)),
            Command::Chat(_) | Command::Sessions(_) | Command::Acp(_) => panic!("run command"),
        }
    }
}
