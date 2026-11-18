//! zx-im: the IM service of zx-agent, the program `zx-im`. A [`Server`] answers each
//! conversation on a chat platform, a direct chat or a thread, with its own session of the
//! agent that `zx-app` configures from config.toml and the agent files, as for the command
//! line; im.toml says which platforms, who may use them and how much runs at once. The
//! program itself (`src/main.rs`) only calls [`main`].
//!
//! A channel only moves messages; the server owns routing, sessions, the run limit and
//! approvals. Supporting another platform means implementing the methods of [`ImChannel`] in
//! a module of `platform`, beside the others, with the helpers there for the duties every
//! platform has.
//!
//! - `channel`: what one platform does for the server ([`ImChannel`]), and the words they
//!   share
//! - `server`: the server, its configuration and what it reports to the operator
//! - `platform`: the platforms, Slack and, with the feature `feishu`, Feishu, and what they
//!   share
//! - `program`: the program: im.toml, its command line, the platforms it configures, the
//!   operator log and the exit codes

mod channel;
mod platform;
mod program;
mod server;

use std::process::ExitCode;

use clap::Parser;

pub use channel::{
    Addressee, ApprovalCard, ApprovalId, CardLabels, ChatKind, Choice, ConversationId, Destination, ErrorReport,
    ImChannel, ImError, Inbound, MessageId, MessageRef, Post, Replacement, Said, ThreadId, UserId,
};
#[cfg(feature = "feishu")]
pub use platform::{FeishuChannel, FeishuConfig};
pub use platform::{SlackChannel, SlackConfig};
pub use server::{ApprovalRecord, Replies, Server, ServerConfig, ServerEvent, ServerSink, SessionKey, StatusStore};

/// Run the program with the command line the process was started with, and return its exit
/// code. A configuration problem found before serving started is written to stderr and exits
/// with the code of a usage error.
pub async fn main() -> ExitCode {
    match program::run(program::Args::parse()).await {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("zx-im: {error:#}");
            ExitCode::from(program::EXIT_USAGE)
        }
    }
}
