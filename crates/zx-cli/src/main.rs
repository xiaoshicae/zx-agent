//! The `zx-agent` command: run, chat, im, sessions and acp.

use std::process::ExitCode;

use clap::Parser;

mod acp;
mod chat;
mod cli;
mod im;
mod list_sessions;
mod process;
mod render;
mod run;
mod session_files;

use cli::{Cli, Command};
use process::EXIT_USAGE;

#[tokio::main]
async fn main() -> ExitCode {
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
