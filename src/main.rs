//! The `zx-agent` command: run, chat, serve, sessions and acp.

use std::process::ExitCode;

use clap::Parser;

mod acp;
mod chat;
mod cli;
mod list_sessions;
mod process;
mod render;
mod run;
mod serve;
mod sessions;

use cli::{Cli, Command};
use process::EXIT_USAGE;

#[tokio::main]
async fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Run(args) => run::run(args).await,
        Command::Chat(args) => chat::run(args).await,
        Command::Serve(args) => serve::run(args).await,
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
