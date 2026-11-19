//! The server and its contracts: the session key, the operator's [`ServerSink`], the reply
//! texts, the configuration, and what every conversation shares.
//!
//! - `router`: takes each platform event and hands it to its conversation's worker
//! - `worker`: one conversation: its session, its runs and what waits for them
//! - `admission`: how much input a conversation may have waiting at once
//! - `outlet`: where a conversation posts, and the session it holds
//! - `approval`: approval requests of a run, as cards in the conversation
//! - `status`: the status message of each run, kept so that a restart can replace it
//! - `received`: each message's receipt until it is handled, and what an earlier process
//!   left pending in the backlog

mod admission;
mod approval;
mod outlet;
mod received;
mod router;
mod status;
mod worker;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Display;
use std::future::Future;
use std::hash::Hash;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use zx_agent::Agent;
use zx_agent::events::ApprovalResult;
use zx_agent::transcript::SessionQuery;
use zx_util::sync::lock;

use crate::{
    ApprovalId, Backlog, ConversationId, Destination, ImChannel, ImError, MemoryBacklog, MessageRef, ThreadId, UserId,
};
use received::{complete_handled, left_pending};
use router::Router;
use status::{left_behind, replace_left_behind};

pub(crate) use status::StatusFiles;
pub use status::StatusStore;

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
    /// conversation's sessions again by asking for them ([`SessionQuery`]). A direct chat has
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

/// The prompt section that tells the model what it cannot see of a conversation the server
/// runs: whose words a message carries, and that of all it writes in a run only the last
/// answer is posted, in place of [`Replies::working`]. The agent's instructions name no host,
/// so the host that builds the agent adds it.
pub(crate) const CONVERSATION_SECTION: &str = "conversation";
pub(crate) const CONVERSATION: &str = "You talk with people through a chat platform. In a group several people write, \
     and each message starts with its sender's name. Only your final answer to a message is posted there, not the \
     text you write before a tool call nor a tool's output, so the final answer must be complete on its own.";

/// Texts the server posts besides the model's answers. The defaults are English; a
/// deployment sets its users' language, for example in im.toml, the configuration of the
/// program `zx-im` (`[replies]`, fields in camelCase, each optional).
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct Replies {
    /// Posted when a run starts; replaced by the answer when it ends.
    pub working: String,
    /// Posted when a conversation waits for a free run slot.
    pub queued: String,
    /// The server or conversation has no capacity for this message; it was not accepted.
    pub overloaded: String,
    /// The only answer in a chat the server does not serve.
    pub unauthorized: String,
    /// The answer to a click by someone who is not an approver.
    pub not_approver: String,
    /// A run cancelled with `/cancel`, and the answer to `/cancel` while waiting for a slot.
    pub cancelled: String,
    /// The answer to `/cancel` when nothing runs.
    pub nothing_to_cancel: String,
    /// The answer to `/new`: the conversation goes on in a new session.
    pub new_session: String,
    /// A run that completed without text.
    pub no_answer: String,
    /// Appended to an answer cut off at the output limit.
    pub truncated: String,
    /// Appended when the model request budget ran out.
    pub model_request_limit: String,
    /// Appended when the token budget ran out.
    pub token_budget: String,
    /// Appended when the cost budget ran out.
    pub cost_budget: String,
    /// A run that reached its time limit.
    pub timed_out: String,
    /// Followed by the error of a failed run.
    pub failed: String,
    /// Followed by the error of a session that could not be opened.
    pub cannot_open: String,
    /// A run that the stop cancelled.
    pub stopped: String,
    /// Replaces, as the server starts, a status that an earlier process left behind when it
    /// ended without replacing it; see [`ServerConfig::statuses`].
    pub interrupted: String,
}

impl Default for Replies {
    fn default() -> Self {
        Self {
            working: "Working…".into(),
            queued: "Queued: as many tasks as allowed are running; this one starts when one ends.".into(),
            overloaded:
                "Too many pending messages or conversations; this message was not accepted. Please try again later."
                    .into(),
            unauthorized: "Not authorized: this chat is not on the service's list.".into(),
            not_approver: "Only listed approvers can allow or deny; this click has no effect.".into(),
            cancelled: "Cancelled.".into(),
            nothing_to_cancel: "Nothing is running.".into(),
            new_session: "Started a new conversation; earlier messages are no longer in its context.".into(),
            no_answer: "(no answer)".into(),
            truncated: "(the answer reached the output limit and was cut off)".into(),
            model_request_limit: "(the model request limit was reached; the task is not finished)".into(),
            token_budget: "(the token budget was reached; the task is not finished)".into(),
            cost_budget: "(the cost budget was reached; the task is not finished)".into(),
            timed_out: "The run timed out; the task is not finished.".into(),
            failed: "The run failed: ".into(),
            cannot_open: "Cannot open the session: ".into(),
            stopped: "The service is stopping: this task was cancelled; please send it again later.".into(),
            interrupted: "The service restarted: this task did not finish; please send it again.".into(),
        }
    }
}

/// Who may use the server, where its sessions work and how much it runs at once.
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
    /// Workers held at once, running, queued and idle included; default 128.
    pub max_conversations: NonZeroUsize,
    /// Pending messages from router admission through inbox, waiting, heard and steering; default 32.
    pub max_pending_messages: NonZeroUsize,
    /// UTF-8 text bytes sharing that admission budget, and the largest inbound message; default 256 KiB.
    pub max_pending_bytes: NonZeroUsize,
    /// How long a conversation may go without input before its session is released.
    pub idle: Duration,
    /// Limit for each send or update, and for each call to [`statuses`](Self::statuses) and to
    /// the [`backlog`](Self::backlog) but [`Backlog::keep`]; a call that takes longer counts
    /// as failed.
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
/// each request in the conversation and answers it when an approver clicks. A session that
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
        let mut router = Router::new(channels, allowed, shared, stop, replacing, receipts, &backlog);
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

/// Settings and resources every conversation reads.
struct Shared {
    /// Makes the session of each conversation, and keeps it open while the conversation
    /// holds it.
    agent: Agent,
    /// Where new sessions work.
    cwd: PathBuf,
    sink: Arc<dyn ServerSink>,
    approvers: HashMap<String, HashSet<UserId>>,
    replies: Replies,
    max_conversations: usize,
    max_pending_messages: NonZeroUsize,
    max_pending_bytes: NonZeroUsize,
    idle: Duration,
    outbound_timeout: Duration,
    /// Run slots of all conversations. Tokio's semaphore is fair: it hands permits out in
    /// the order they were requested.
    permits: Arc<Semaphore>,
    statuses: Option<Arc<dyn StatusStore>>,
    /// The names of users looked up so far, by platform; a user's ID stands for a name the
    /// platform did not give. As many as there are people writing to the bot.
    names: Mutex<HashMap<(String, UserId), String>>,
    /// The session each conversation the bot took part in goes on in: its latest. Read from
    /// the storage's headers once, at the start, and kept as the server makes sessions; only
    /// this process makes sessions for these conversations. As many as there are
    /// conversations the bot ever took part in.
    sessions: Mutex<HashMap<SessionKey, String>>,
}

impl Shared {
    /// What the conversations share, from everything in `config` but the chats served and the
    /// backlog.
    fn new(config: ServerConfig, agent: Agent, sink: Arc<dyn ServerSink>) -> Self {
        let ServerConfig {
            cwd,
            // Taken by the router before.
            conversations: _,
            approvers,
            max_running,
            max_conversations,
            max_pending_messages,
            max_pending_bytes,
            idle,
            outbound_timeout,
            replies,
            statuses,
            // The channels' and the receipts', through the router.
            backlog: _,
        } = config;
        Self {
            agent,
            cwd,
            sink,
            approvers: group(approvers),
            replies,
            idle,
            outbound_timeout,
            permits: Arc::new(Semaphore::new(max_running.get())),
            max_conversations: max_conversations.get(),
            max_pending_messages,
            max_pending_bytes,
            statuses,
            names: Mutex::default(),
            sessions: Mutex::default(),
        }
    }

    /// Find the session of each conversation the storage holds: the most recently started
    /// of those with its labels. Run once, before any message is routed.
    async fn index_sessions(&self) -> Result<(), ImError> {
        let headers = self
            .agent
            .session_headers(&SessionQuery::default())
            .await
            .map_err(|error| ImError::Config(format!("cannot read the sessions of the conversations: {error:#}")))?;
        let mut sessions = lock(&self.sessions);
        // Most recently started first: the first of a conversation is its latest.
        for header in headers {
            if let Some(key) = SessionKey::of_labels(&header.labels) {
                sessions.entry(key).or_insert(header.id);
            }
        }
        Ok(())
    }

    /// The session the conversation `key` goes on in, if the bot ever took part in it.
    fn session_of(&self, key: &SessionKey) -> Option<String> {
        lock(&self.sessions).get(key).cloned()
    }

    /// The conversation `key` goes on in the session `id` from now on: one just made for it.
    fn go_on_in(&self, key: &SessionKey, id: &str) {
        lock(&self.sessions).insert(key.clone(), id.to_string());
    }

    /// The name people see for `user` on `channel`, asked once and then remembered. The user's
    /// ID when the platform gives no name; a failed lookup is reported against `key`, once.
    async fn name_of(&self, channel: &dyn ImChannel, key: &SessionKey, user: &UserId) -> String {
        let known = (channel.platform().to_string(), user.clone());
        if let Some(name) = lock(&self.names).get(&known) {
            return name.clone();
        }
        let act = format!("look up the name of {user}");
        let name = self
            .bounded(Some(key), &act, channel.display_name(user))
            .await
            .flatten()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| user.0.clone());
        lock(&self.names).insert(known, name.clone());
        name
    }

    fn is_approver(&self, platform: &str, user: &UserId) -> bool {
        self.approvers.get(platform).is_some_and(|users| users.contains(user))
    }

    fn failed(&self, key: Option<&SessionKey>, error: String) {
        self.sink.emit(&ServerEvent::Failed {
            key: key.cloned(),
            error,
        });
    }

    /// `operation` within the outbound timeout. A failure, or the time running out, is
    /// reported to the operator against `key` as [`timed`] words it, and leaves nothing.
    async fn bounded<T, E: Display>(
        &self,
        key: Option<&SessionKey>,
        act: &str,
        operation: impl Future<Output = Result<T, E>>,
    ) -> Option<T> {
        match timed(self.outbound_timeout, act, operation).await {
            Ok(value) => Some(value),
            Err(error) => {
                self.failed(key, error);
                None
            }
        }
    }

    fn statuses(&self) -> Option<&dyn StatusStore> {
        self.statuses.as_deref()
    }

    /// Keep the status of a run that starts. A failure is reported; the run goes on.
    async fn keep_status(&self, key: &SessionKey, status: &MessageRef) {
        if let Some(statuses) = self.statuses() {
            let act = format!("keep status {}", status.id);
            self.bounded(Some(key), &act, statuses.insert(key, status)).await;
        }
    }

    /// Forget a status about to be replaced. A failure is reported.
    async fn forget_status(&self, key: &SessionKey, status: &MessageRef) {
        if let Some(statuses) = self.statuses() {
            let act = format!("forget status {}", status.id);
            self.bounded(Some(key), &act, statuses.remove(key, status)).await;
        }
    }
}

/// `operation` within `timeout`. The error says what could not be done: `cannot <act>:
/// <error>`, or `cannot <act>: took longer than <n> s`.
async fn timed<T, E: Display>(
    timeout: Duration,
    act: &str,
    operation: impl Future<Output = Result<T, E>>,
) -> Result<T, String> {
    match tokio::time::timeout(timeout, operation).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(format!("cannot {act}: {error}")),
        Err(_) => Err(format!("cannot {act}: took longer than {} s", timeout.as_secs())),
    }
}

/// `pairs` by platform.
fn group<T: Eq + Hash>(pairs: Vec<(String, T)>) -> HashMap<String, HashSet<T>> {
    let mut grouped: HashMap<String, HashSet<T>> = HashMap::new();
    for (platform, item) in pairs {
        grouped.entry(platform).or_default().insert(item);
    }
    grouped
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
