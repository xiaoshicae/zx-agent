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
//! Every way in builds its agent in one place, which knows neither a command line nor a
//! terminal:
//!
//! - `assembly`: an agent made from config.toml, the agent file and what one start says
//!   over them; what each session gets for itself; where a mode keeps its sessions
//!
//! The ways in do not refer to each other; each depends on the three above:
//!
//! - `run`, `chat`, `acp`, `im`, `list_sessions`: the subcommands
//! - `cli`: the arguments of the subcommands
//! - `listing`: how a listing shows sessions
//! - `process`: exit codes, signals, and what every subcommand writes to its outputs
//! - `render`: how calls, results and times are shown

pub mod config;
pub mod otel;

mod acp;
mod assembly;
mod chat;
mod cli;
mod im;
mod list_sessions;
mod listing;
mod process;
mod render;
mod run;

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
