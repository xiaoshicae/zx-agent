//! zx-im: the IM service of zx-agent, the program `zx-im`. A [`Server`] answers each
//! conversation on a chat platform, a direct chat or a thread, with its own session of the
//! agent that `zx-app` configures from config.toml and the agent files, as for the command
//! line; im.toml says which platforms, who may use them, which chats have an agent or
//! approvers of their own, and how much runs at once and in a day. The program itself
//! (`src/main.rs`) only calls [`main`].
//!
//! A channel only moves messages; the server owns routing, sessions, the run limit and
//! approvals. Supporting another platform means implementing the methods of [`ImChannel`] in
//! a module of `platform`, beside the others, with the helpers there for the duties every
//! platform has, and a [`PlatformFactory`] that builds the channel from its im.toml section.
//! A host's own program adds a platform outside this crate with
//! [`Program::register_platform`].
//!
//! - `channel`: what one platform does for the server ([`ImChannel`]), where it puts what it
//!   receives ([`Inlet`]), and the words they share
//! - `backlog`: each message from its arrival until it is handled ([`Backlog`])
//! - `server`: the server, its configuration and what it reports to the operator
//! - `platform`: the platforms, Slack and, with the feature `feishu`, Feishu, what they
//!   share, and the factories that build them from im.toml
//! - `files`: one record per file, as the program's stores on disk write them
//! - `postgres`, with the feature `postgres`: the program's stores in Postgres, which the
//!   replicas of a deployment share
//! - `program`: the program ([`Program`]): im.toml, its command line, the platforms it
//!   configures, the operator log and the exit codes
//! - `test_support`, compiled for tests only: what the unit tests of several modules share

mod backlog;
mod channel;
mod files;
mod platform;
#[cfg(feature = "postgres")]
mod postgres;
mod program;
mod server;
#[cfg(test)]
mod test_support;

use std::process::ExitCode;

pub use self::backlog::{Backlog, Kept, MemoryBacklog, MessageKey};
pub use self::channel::{
    Addressee, ApprovalCard, ApprovalId, Attachment, CardLabels, ChatKind, Choice, ConversationId, Destination,
    ErrorReport, ImChannel, ImError, ImageRef, Inbound, Inlet, MessageId, MessageRef, Post, Replacement, Said,
    ThreadId, UserId,
};
pub use self::platform::{BuiltChannel, PlatformFactory, PlatformInput, SlackChannel, SlackConfig};
#[cfg(feature = "feishu")]
pub use self::platform::{FeishuChannel, FeishuConfig};
pub use self::program::Program;
pub use self::server::{
    Access, ApprovalRecord, ChatConfig, ChatUsage, DailyLimit, InvalidDailyLimit, Replies, RunEnd, RunRecord, Server,
    ServerConfig, ServerEvent, ServerSink, SessionKey, StatusStore, Trigger, Triggers, UsageStore,
};

/// Run the program with its built-in platforms ([`Program::run`]).
pub async fn main() -> ExitCode {
    Program::new().run().await
}
