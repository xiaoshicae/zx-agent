//! The command line of zx-agent: the program `zx-agent` with its subcommands `run`, `chat`,
//! `acp` and `sessions`, built on the `zx-agent` library. The program itself (`src/main.rs`)
//! only calls [`main`].
//!
//! `run`, `chat` and `acp` get their agent from `zx-app`, which reads config.toml and the
//! agent files, runs the process's shutdown and knows neither a command line nor a terminal;
//! `sessions` uses it only to find the session directory. The subcommands do not refer to
//! each other:
//!
//! - `run`, `chat`, `acp`, `sessions`: the subcommands
//! - `cli`: the arguments of the subcommands
//! - `application`: the application a subcommand runs on, and the sessions of its agents
//! - `session_file`: `--session`'s file, whose session run and chat continue
//! - `process`: exit codes, JSON lines on stdout, and what every subcommand says of signals
//!   and of the trace export
//! - `render`: how calls, results, times and sessions are shown
//! - `test_support`, compiled for tests only: what the unit tests of several modules share

mod acp;
mod application;
mod chat;
mod cli;
mod process;
mod render;
mod run;
mod session_file;
mod sessions;
#[cfg(test)]
mod test_support;

use std::process::ExitCode;

use clap::Parser;
use zx_util::stdio::eprint_line;

use self::cli::{Cli, Command};
use self::process::EXIT_USAGE;

/// Run the command line the process was started with, and return its exit code. An error
/// that ends a subcommand, such as a configuration problem found before it started or an
/// ACP connection that failed, is written to stderr and exits with the code of a usage
/// error.
pub async fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Run(args) => run::run(args).await,
        Command::Chat(args) => chat::run(args).await,
        Command::Sessions(args) => sessions::run(args).await,
        Command::Acp(args) => acp::run(args).await,
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            // The causes may quote a server, a file or the environment.
            eprint_line(&format!("zx-agent: {}", render::visible(&format!("{error:#}"))));
            ExitCode::from(EXIT_USAGE)
        }
    }
}
