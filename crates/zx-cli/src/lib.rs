//! The command line of zx-agent: the program `zx-agent` with its subcommands `run`, `chat`,
//! `acp`, `im` and `sessions`, built on the `zx-agent` library. The program itself
//! (`src/main.rs`) only calls [`main`].
//!
//! What does not depend on one invocation is public, for the program's tests and for
//! another way in that is configured the same:
//!
//! - [`config`]: `config.toml`, the environment: the model, its provider and where commands
//!   run; and the agents, as files: instructions, tools, permissions and limits
//! - [`otel`]: OpenTelemetry traces of runs, exported when an OTLP endpoint is configured
//!
//! The rest is the program:
//!
//! - `cli`: the arguments, and what the subcommands share: the settings, the agent of a
//!   mode and what each of its sessions gets
//! - `run`, `chat`, `acp`, `im`, `list_sessions`: the subcommands
//! - `session_files`: where every mode keeps its sessions, and how a listing shows them
//! - `process`: exit codes, signals, and what every subcommand writes to its outputs
//! - `render`: how calls, results and times are shown

pub mod config;
pub mod otel;

mod acp;
mod chat;
mod cli;
mod im;
mod list_sessions;
mod process;
mod render;
mod run;
mod session_files;

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
        Command::Im(args) => im::run(args).await,
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
