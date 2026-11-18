//! The command line of zx-agent: the program `zx-agent` with its subcommands `run`, `chat`,
//! `acp` and `sessions`, built on the `zx-agent` library. The program itself (`src/main.rs`)
//! only calls [`main`].
//!
//! `run`, `chat` and `acp` get their agent from `zx-app`, which reads config.toml and the
//! agent files, runs the process's shutdown and knows neither a command line nor a terminal;
//! `sessions` uses it only to find the session directory. The subcommands do not refer to
//! each other:
//!
//! - `run`, `chat`, `acp`, `list_sessions`: the subcommands
//! - `cli`: the arguments of the subcommands
//! - `session_file`: `--session`'s file, whose session run and chat continue
//! - `listing`: how a listing shows sessions
//! - `process`: exit codes, and what every subcommand writes to its outputs, about signals and
//!   the trace export too
//! - `render`: how calls, results and times are shown

mod acp;
mod chat;
mod cli;
mod list_sessions;
mod listing;
mod process;
mod render;
mod run;
mod session_file;

use std::process::ExitCode;

use clap::Parser;

use cli::{Cli, Command};
use process::EXIT_USAGE;

/// Run the command line the process was started with, and return its exit code. A
/// configuration problem found before the subcommand started is written to stderr and
/// exits with the code of a usage error.
pub async fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Run(args) => run::run(args).await,
        Command::Chat(args) => chat::run(args).await,
        Command::Sessions(args) => list_sessions::run(args).await,
        Command::Acp(args) => acp::run(args).await,
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("zx-agent: {e:#}");
            ExitCode::from(EXIT_USAGE)
        }
    }
}
