//! The server and its contracts: the session key, the operator's [`ServerSink`], the
//! configuration, and the server itself.
//!
//! - `replies`: the texts the server posts besides the model's answers
//! - `shared`: what every conversation shares: settings, the agent, the run slots and the
//!   outbound timeout
//! - `router`: takes each platform event and hands it to its conversation's worker
//! - `worker`: one conversation: its session, its runs and what waits for them
//! - `admission`: how much input a conversation may have waiting at once
//! - `outlet`: where a conversation posts, and the session it holds
//! - `render`: the text that says how a conversation's work ended
//! - `approval`: approval requests of a run, as cards in the conversation
//! - `progress`: what a run tells its conversation while it goes on: its approvals, and the
//!   answers it goes on from
//! - `status`: the status message of each run, kept so that a restart can replace it
//! - `received`: each message's receipt until it is handled, and what an earlier process
//!   left pending in the backlog

mod admission;
mod approval;
mod outlet;
mod progress;
mod received;
mod render;
mod replies;
mod router;
mod shared;
mod status;
mod worker;

use std::collections::{BTreeMap, HashSet};
use std::hash::Hash;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use zx_agent::Agent;
use zx_agent::events::ApprovalResult;

use self::received::{complete_handled, left_pending};
pub use self::replies::Replies;
pub(crate) use self::replies::{CONVERSATION, CONVERSATION_SECTION};
use self::router::{Router, RouterSetup};
use self::shared::{Shared, group, timed};
pub(crate) use self::status::StatusFiles;
pub use self::status::StatusStore;
use self::status::{left_behind, replace_left_behind};
use crate::{ApprovalId, Backlog, ConversationId, Destination, ImChannel, ImError, MemoryBacklog, ThreadId, UserId};

const DEFAULT_MAX_RUNNING: NonZeroUsize = NonZeroUsize::new(4).expect("4 is not zero");
const DEFAULT_MAX_CONVERSATIONS: NonZeroUsize = NonZeroUsize::new(128).expect("128 is not zero");
const DEFAULT_PENDING_MESSAGES: NonZeroUsize = NonZeroUsize::new(32).expect("32 is not zero");
const DEFAULT_PENDING_BYTES: NonZeroUsize = NonZeroUsize::new(256 * 1024).expect("256 KiB is not zero");
const DEFAULT_IDLE: Duration = Duration::from_secs(30 * 60);
/// Long enough for a platform's rate-limit queue, short enough that a platform that stopped
/// answering does not hold a conversation for minutes.
const DEFAULT_OUTBOUND_TIMEOUT: Duration = Duration::from_secs(30);

/// One conversation, with a session of its own: a direct chat, or a thread in a group.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub platform: String,
    pub conversation: ConversationId,
    /// The thread in a group; `None` for a direct chat, which is one conversation as a whole.
    pub thread: Option<ThreadId>,
}

/// The labels of a conversation's sessions: its platform, its chat, and its thread, which is
/// empty for a direct chat.
const PLATFORM_LABEL: &str = "im.platform";
const CONVERSATION_LABEL: &str = "im.conversation";
const THREAD_LABEL: &str = "im.thread";

impl SessionKey {
    /// The labels of this conversation's sessions, whatever storage the agent keeps them in:
    /// the server gives them to each session it creates for the conversation, and finds the
    /// conversation's sessions again by asking for them
    /// ([`SessionQuery`](zx_agent::transcript::SessionQuery)). A direct chat has
    /// an empty thread label, so that asking for its sessions never finds a thread's.
    pub fn labels(&self) -> BTreeMap<String, String> {
        let thread = self.thread.as_ref().map(|thread| thread.0.clone());
        BTreeMap::from([
            (PLATFORM_LABEL.to_string(), self.platform.clone()),
            (CONVERSATION_LABEL.to_string(), self.conversation.0.clone()),
            (THREAD_LABEL.to_string(), thread.unwrap_or_default()),
        ])
    }

    /// The conversation whose sessions carry `labels`, as [`labels`](Self::labels) gives them;
    /// `None` for a session of something else.
    fn of_labels(labels: &BTreeMap<String, String>) -> Option<Self> {
        let thread = labels.get(THREAD_LABEL)?;
        Some(Self {
            platform: labels.get(PLATFORM_LABEL)?.clone(),
            conversation: ConversationId(labels.get(CONVERSATION_LABEL)?.clone()),
            thread: (!thread.is_empty()).then(|| ThreadId(thread.clone())),
        })
    }

    /// Where the server posts for this conversation: the direct chat, or the thread.
    fn destination(&self) -> Destination {
        Destination {
            conversation: self.conversation.clone(),
            thread: self.thread.clone(),
        }
    }
}

/// Receives what the server reports besides chat messages. Called from the server's tasks;
/// must return promptly.
pub trait ServerSink: Send + Sync {
    fn emit(&self, event: &ServerEvent);
}

/// Something for the operator.
#[derive(Clone, Debug, PartialEq)]
pub enum ServerEvent {
    /// An approval ended: who decided what.
    Approval(ApprovalRecord),
    /// An idle conversation released its session; the next message opens it again.
    Released { key: SessionKey },
    /// A failure nobody in the chat sees, such as a reply that could not be posted.
    Failed { key: Option<SessionKey>, error: String },
}

/// One ended approval.
#[derive(Clone, Debug, PartialEq)]
pub struct ApprovalRecord {
    pub key: SessionKey,
    pub tool_call_id: String,
    pub tool: String,
    /// The card; `None` when none was posted, for example because posting failed.
    pub approval: Option<ApprovalId>,
    /// Who clicked; `None` when nobody decided: timeout, cancellation or a failure.
    pub user: Option<UserId>,
    pub result: ApprovalResult,
    /// When it ended, in milliseconds since the Unix epoch.
    pub ended_ms: u64,
}

/// Who may use the server, where its sessions work and how much it runs at once. [`Default`]
/// sets the limits the program `zx-im` has unless its im.toml sets them
/// (docs/CONFIGURATION.md).
pub struct ServerConfig {
    /// Where the session of a new conversation works, and goes on working when it is
    /// opened again; the current directory by default.
    pub cwd: PathBuf,
    /// Chats the server answers in, per platform. A message from another chat gets
    /// [`Replies::unauthorized`] and nothing else.
    pub conversations: Vec<(String, ConversationId)>,
    /// Users whose clicks decide approvals, per platform. A click by anyone else gets
    /// [`Replies::not_approver`] and decides nothing.
    pub approvers: Vec<(String, UserId)>,
    /// Conversations that may run at once; the others wait in arrival order.
    pub max_running: NonZeroUsize,
    /// Conversations served at once: running, waiting for a run slot, or idle until their
    /// session is released. A message that would start one more is not accepted
    /// ([`Replies::overloaded`]).
    pub max_conversations: NonZeroUsize,
    /// Messages, other than its commands, that a conversation may have pending at once: from
    /// their arrival until its session records them or they are dropped. One more is not
    /// accepted.
    pub max_pending_messages: NonZeroUsize,
    /// UTF-8 bytes of text that a conversation's pending messages may hold together, and so
    /// the most one message may hold.
    pub max_pending_bytes: NonZeroUsize,
    /// How long a conversation may go without input before its session is released.
    pub idle: Duration,
    /// Limit for each call to a channel but [`ImChannel::run`], and for each call to
    /// [`statuses`](Self::statuses) and to the [`backlog`](Self::backlog) but
    /// [`Backlog::keep`]; a call that takes longer counts as failed.
    pub outbound_timeout: Duration,
    pub replies: Replies,
    /// Keeps the status of each run while it runs, so that a crash or a kill does not leave
    /// threads saying that the work goes on.
    ///
    /// The next start replaces those statuses with [`Replies::interrupted`]. `None`, the
    /// default, keeps nothing; the program `zx-im` keeps them in files under its session
    /// directory.
    pub statuses: Option<Arc<dyn StatusStore>>,
    /// Keeps each message from its arrival until it is handled, and knows a message delivered
    /// again ([`Backlog`]). [`MemoryBacklog`] by default, which keeps nothing across a restart;
    /// the program `zx-im` keeps its backlog in files under its session directory.
    pub backlog: Arc<dyn Backlog>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            cwd: PathBuf::from("."),
            conversations: Vec::new(),
            approvers: Vec::new(),
            max_running: DEFAULT_MAX_RUNNING,
            max_conversations: DEFAULT_MAX_CONVERSATIONS,
            max_pending_messages: DEFAULT_PENDING_MESSAGES,
            max_pending_bytes: DEFAULT_PENDING_BYTES,
            idle: DEFAULT_IDLE,
            outbound_timeout: DEFAULT_OUTBOUND_TIMEOUT,
            replies: Replies::default(),
            statuses: None,
            backlog: Arc::new(MemoryBacklog::default()),
        }
    }
}

/// Answers chat messages, one session per conversation, as a person in the chat would.
///
/// The sessions are the agent's: it chooses the model, tools, policies and limits, and keeps
/// the sessions in its storage, where [`SessionKey::labels`] marks those of a conversation.
/// Their approvals must wait for the host
/// ([`ApprovalMode::Wait`](zx_agent::policy::ApprovalMode::Wait)): the server posts a card for
/// each request in the conversation and resolves it when an approver clicks. A session that
/// cannot be opened is reported to the conversation after [`Replies::cannot_open`]; the next
/// message tries again.
///
/// - A direct chat is one conversation. `/new` alone starts a new session for the
///   conversation at once, and the earlier ones are kept: a run in progress is cancelled
///   first, and what is asked before that run has ended is answered in the new session.
/// - In a group, a message that mentions the bot starts a thread under it, and each thread
///   is a conversation. The first mention in a thread someone else started reads what was
///   said there before ([`ImChannel::thread_history`]), unless `/new` came first.
/// - In a thread the bot takes part in, a message is answered when it mentions the bot, or
///   when it comes from the person the bot just answered, mentions no one, and nobody else
///   spoke in between. Other messages there are only recorded, for the next answer.
///   Messages from a group reach the model with their sender's name.
/// - A conversation that is already running takes the messages it answers as steering, and
///   acknowledges them ([`ImChannel::acknowledge`]).
/// - `/cancel` alone cancels the conversation's run.
/// - At most [`ServerConfig::max_running`] conversations run at once.
/// - Approval requests become cards; a click by an approver decides, and the card shows the
///   result however the approval ended.
pub struct Server {
    config: ServerConfig,
    agent: Agent,
    sink: Arc<dyn ServerSink>,
}

impl Server {
    /// A server for `config` whose conversations are sessions of `agent`, and which reports
    /// to `sink`.
    pub fn new(config: ServerConfig, agent: Agent, sink: Arc<dyn ServerSink>) -> Self {
        Self { config, agent, sink }
    }

    /// Serve `channels` until `cancel` is triggered or a channel fails.
    ///
    /// Stopping cancels the runs and waits until each conversation has posted how it ended,
    /// [`Replies::stopped`] for a cancelled run. A message not handled by then, waiting for a
    /// run slot, steered into a run that did not take it in, or received during the stop,
    /// stays pending in the [`backlog`](ServerConfig::backlog). A channel's error is then
    /// returned. Two channels with the same platform name are a configuration error.
    ///
    /// As the server starts, it handles what the backlog holds pending, in the order kept,
    /// before anything new. With [`ServerConfig::statuses`], the statuses an earlier process
    /// left behind are read before anything is received, then replaced with
    /// [`Replies::interrupted`] while the server runs: each is tried once and then forgotten.
    /// The stop gives up on the rest, which stay for the next start.
    pub async fn run(self, channels: Vec<Arc<dyn ImChannel>>, cancel: &CancellationToken) -> Result<(), ImError> {
        distinct_platforms(&channels)?;
        let Server {
            mut config,
            agent,
            sink,
        } = self;
        // The chats served are the router's; the rest of the configuration is shared.
        let allowed = group(std::mem::take(&mut config.conversations));
        let backlog = Arc::clone(&config.backlog);
        let timeout = config.outbound_timeout;
        let shared = Arc::new(Shared::new(config, agent, Arc::clone(&sink)));
        shared.index_sessions().await?;
        // Read before any run can start, so that no status of this process is among them.
        let left = left_behind(&shared).await;
        // Read before any channel runs, so that nothing kept by this process is among them.
        let pending = left_pending(&shared, backlog.as_ref()).await;
        let stop = cancel.child_token();
        let replacing = tokio::spawn(replace_left_behind(
            left,
            channels.clone(),
            Arc::clone(&shared),
            stop.clone(),
        ));
        let (receipts, completing) = complete_handled(Arc::clone(&backlog), Arc::clone(&sink), timeout, stop.clone());
        let mut router = Router::new(RouterSetup {
            channels,
            allowed,
            shared,
            stop,
            replacing,
            receipts,
            backlog,
        });
        router.replay(pending);
        let result = router.run().await;
        // Every receipt is gone with the router: the task completes what was handled before
        // the stop, and ends.
        if let Err(error) = completing.await {
            sink.emit(&ServerEvent::Failed {
                key: None,
                error: format!("completing handled messages in the backlog failed: {error}"),
            });
        }
        result
    }
}

/// An error when two of `channels` have the same platform name.
fn distinct_platforms(channels: &[Arc<dyn ImChannel>]) -> Result<(), ImError> {
    let mut platforms = HashSet::new();
    for channel in channels {
        if !platforms.insert(channel.platform()) {
            return Err(ImError::Config(format!(
                "more than one channel is named {:?}",
                channel.platform()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(platform: &str, conversation: &str, thread: Option<&str>) -> SessionKey {
        SessionKey {
            platform: platform.into(),
            conversation: ConversationId(conversation.into()),
            thread: thread.map(|thread| ThreadId(thread.into())),
        }
    }

    #[test]
    fn a_thread_s_labels_are_not_its_chat_s() {
        let labels = |key: SessionKey| -> Vec<(String, String)> { key.labels().into_iter().collect() };
        let pair = |name: &str, value: &str| (name.to_string(), value.to_string());

        assert_eq!(
            labels(key("slack", "C1", Some("t1"))),
            [
                pair("im.conversation", "C1"),
                pair("im.platform", "slack"),
                pair("im.thread", "t1")
            ]
        );
        assert_eq!(
            labels(key("slack", "D1", None)),
            [
                pair("im.conversation", "D1"),
                pair("im.platform", "slack"),
                pair("im.thread", "")
            ]
        );
    }
}
