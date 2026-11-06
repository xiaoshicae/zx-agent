//! The server and its contracts: the session key, the host's [`SessionFactory`], the
//! operator's [`ServerSink`], the reply texts, the configuration, and what every
//! conversation shares. Routing is in `router.rs`.

use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::future::Future;
use std::hash::Hash;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::AgentSession;
use crate::agent::ApprovalResult;
use crate::interaction::Interaction;

use super::router::Router;
use super::status::{left_behind, replace_left_behind};
use super::{ApprovalId, ConversationId, Destination, ImChannel, ImError, MessageRef, StatusStore, ThreadId, UserId};

const DEFAULT_MAX_RUNNING: NonZeroUsize = NonZeroUsize::new(4).expect("4 is not zero");
const DEFAULT_IDLE: Duration = Duration::from_secs(30 * 60);
/// Long enough for a platform's rate-limit queue, short enough that a platform that stopped
/// answering does not hold a conversation for minutes.
const DEFAULT_OUTBOUND_TIMEOUT: Duration = Duration::from_secs(30);

/// One conversation: a thread in a chat on a platform, with a session of its own.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub platform: String,
    pub conversation: ConversationId,
    pub thread: ThreadId,
}

impl SessionKey {
    /// The session file of this conversation under `dir`:
    /// `<dir>/<platform>/<conversation>/<thread>.jsonl`. Each part is encoded so that no ID
    /// can name another directory: bytes other than ASCII letters, digits, `-` and `_`
    /// become `%XX`, and an empty part becomes `%`.
    pub fn file_in(&self, dir: &Path) -> PathBuf {
        let mut path = dir.join(segment(&self.platform));
        path.push(segment(&self.conversation.0));
        path.push(format!("{}.jsonl", segment(&self.thread.0)));
        path
    }

    /// The thread, where the server posts for this conversation.
    pub(crate) fn destination(&self) -> Destination {
        Destination {
            conversation: self.conversation.clone(),
            thread: Some(self.thread.clone()),
        }
    }
}

/// `id` as one part of a path, encoded as [`SessionKey::file_in`] says.
pub(crate) fn segment(id: &str) -> String {
    if id.is_empty() {
        return "%".to_string();
    }
    let mut encoded = String::with_capacity(id.len());
    for byte in id.bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Opens the session of a conversation. The host chooses the model, tools, policies, limits
/// and where sessions are stored.
#[async_trait]
pub trait SessionFactory: Send + Sync {
    /// Open the session for `key`, creating it on first use. Set `interaction` as the loop's
    /// [`interaction`](crate::agent::LoopConfig::interaction): it posts approval cards to the
    /// thread. Called again after an idle session was released. An error is posted to the
    /// thread after [`Replies::cannot_open`]; the next message tries again.
    async fn open(&self, key: &SessionKey, interaction: Arc<dyn Interaction>) -> Result<AgentSession, String>;
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

/// Texts the server posts besides the model's answers. The defaults are English; a
/// deployment sets its users' language, for example in the configuration of `serve`
/// (`[replies]`, fields in camelCase, each optional).
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct Replies {
    /// Posted when a run starts; replaced by the answer when it ends.
    pub working: String,
    /// Posted when a conversation waits for a free run slot.
    pub queued: String,
    /// The only answer in a chat the server does not serve.
    pub unauthorized: String,
    /// The answer to a click by someone who is not an approver.
    pub not_approver: String,
    /// A run cancelled with `/cancel`, and the answer to `/cancel` while waiting for a slot.
    pub cancelled: String,
    /// The answer to `/cancel` when nothing runs.
    pub nothing_to_cancel: String,
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
    /// Posted once in a thread when the server stops before running some of its messages:
    /// those waiting for a run slot, steering the run did not take in, and those that
    /// arrived during the stop.
    pub stopping: String,
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
            unauthorized: "Not authorized: this chat is not on the service's list.".into(),
            not_approver: "Only listed approvers can allow or deny; this click has no effect.".into(),
            cancelled: "Cancelled.".into(),
            nothing_to_cancel: "Nothing is running.".into(),
            no_answer: "(no answer)".into(),
            truncated: "(the answer reached the output limit and was cut off)".into(),
            model_request_limit: "(the model request limit was reached; the task is not finished)".into(),
            token_budget: "(the token budget was reached; the task is not finished)".into(),
            cost_budget: "(the cost budget was reached; the task is not finished)".into(),
            timed_out: "The run timed out; the task is not finished.".into(),
            failed: "The run failed: ".into(),
            cannot_open: "Cannot open the session: ".into(),
            stopping: "The service is stopping: messages of this thread not handled yet will not be; please send them again later.".into(),
            stopped: "The service is stopping: this task was cancelled; please send it again later.".into(),
            interrupted: "The service restarted: this task did not finish; please send it again.".into(),
        }
    }
}

/// Who may use the server and how much it runs at once.
pub struct ServerConfig {
    /// Chats the server answers in, per platform. A message from another chat gets
    /// [`Replies::unauthorized`] and nothing else.
    pub conversations: Vec<(String, ConversationId)>,
    /// Users whose clicks decide approvals, per platform. A click by anyone else gets
    /// [`Replies::not_approver`] and decides nothing.
    pub approvers: Vec<(String, UserId)>,
    /// Conversations that may run at once; the others wait in arrival order.
    pub max_running: NonZeroUsize,
    /// How long a conversation may go without input before its session is released.
    pub idle: Duration,
    /// Limit for each send or update, and for each call to [`statuses`](Self::statuses); a call
    /// that takes longer counts as failed.
    pub outbound_timeout: Duration,
    pub replies: Replies,
    /// Keeps the status of each run while it runs, so that a crash or a kill does not leave
    /// threads saying that the work goes on.
    ///
    /// The next start replaces those statuses with [`Replies::interrupted`]. `None`, the
    /// default, keeps nothing. [`StatusFiles`](super::StatusFiles) keeps them in a directory.
    pub statuses: Option<Arc<dyn StatusStore>>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            conversations: Vec::new(),
            approvers: Vec::new(),
            max_running: DEFAULT_MAX_RUNNING,
            idle: DEFAULT_IDLE,
            outbound_timeout: DEFAULT_OUTBOUND_TIMEOUT,
            replies: Replies::default(),
            statuses: None,
        }
    }
}

/// Answers chat messages with one session per thread.
///
/// - A message in a chat starts a thread under it; replies in that thread continue the
///   same session. A conversation that is already running takes new messages as steering.
/// - `/cancel` alone in a thread cancels its run.
/// - At most [`ServerConfig::max_running`] conversations run at once.
/// - Approval requests become cards; a click by an approver decides, and the card shows the
///   result however the approval ended.
pub struct Server {
    config: ServerConfig,
    factory: Arc<dyn SessionFactory>,
    sink: Arc<dyn ServerSink>,
}

impl Server {
    /// A server for `config` that opens sessions with `factory` and reports to `sink`.
    pub fn new(config: ServerConfig, factory: Arc<dyn SessionFactory>, sink: Arc<dyn ServerSink>) -> Self {
        Self { config, factory, sink }
    }

    /// Serve `channels` until `cancel` is triggered or a channel fails.
    ///
    /// Stopping cancels the runs and waits until each conversation has posted how it ended,
    /// [`Replies::stopped`] for a cancelled run; a thread with messages that will not run,
    /// including those the channels received during the stop, also gets
    /// [`Replies::stopping`]. A channel's error is then returned. Two channels with the same
    /// platform name are a configuration error.
    ///
    /// With [`ServerConfig::statuses`], the statuses an earlier process left behind are read
    /// before anything is received, then replaced with [`Replies::interrupted`] while the
    /// server runs: each is tried once and then forgotten. The stop gives up on the rest,
    /// which stay for the next start.
    pub async fn run(self, channels: Vec<Arc<dyn ImChannel>>, cancel: &CancellationToken) -> Result<(), ImError> {
        distinct_platforms(&channels)?;
        let Server {
            mut config,
            factory,
            sink,
        } = self;
        // The chats served are the router's; the rest of the configuration is shared.
        let allowed = group(std::mem::take(&mut config.conversations));
        let shared = Arc::new(Shared::new(config, factory, sink));
        // Read before any run can start, so that no status of this process is among them.
        let left = left_behind(&shared).await;
        let stop = cancel.child_token();
        let replacing = tokio::spawn(replace_left_behind(
            left,
            channels.clone(),
            Arc::clone(&shared),
            stop.clone(),
        ));
        Router::new(channels, allowed, shared, stop, replacing).run().await
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
pub(crate) struct Shared {
    pub(crate) factory: Arc<dyn SessionFactory>,
    pub(crate) sink: Arc<dyn ServerSink>,
    approvers: HashMap<String, HashSet<UserId>>,
    pub(crate) replies: Replies,
    pub(crate) idle: Duration,
    pub(crate) outbound_timeout: Duration,
    /// Run slots of all conversations. Tokio's semaphore is fair: it hands permits out in
    /// the order they were requested.
    pub(crate) permits: Arc<Semaphore>,
    statuses: Option<Arc<dyn StatusStore>>,
}

impl Shared {
    /// What the conversations share, from everything in `config` but the chats served.
    pub(crate) fn new(config: ServerConfig, factory: Arc<dyn SessionFactory>, sink: Arc<dyn ServerSink>) -> Self {
        let ServerConfig {
            // Taken by the router before.
            conversations: _,
            approvers,
            max_running,
            idle,
            outbound_timeout,
            replies,
            statuses,
        } = config;
        Self {
            factory,
            sink,
            approvers: group(approvers),
            replies,
            idle,
            outbound_timeout,
            permits: Arc::new(Semaphore::new(max_running.get())),
            statuses,
        }
    }

    pub(crate) fn is_approver(&self, platform: &str, user: &UserId) -> bool {
        self.approvers.get(platform).is_some_and(|users| users.contains(user))
    }

    pub(crate) fn failed(&self, key: Option<&SessionKey>, error: String) {
        self.sink.emit(&ServerEvent::Failed {
            key: key.cloned(),
            error,
        });
    }

    /// `operation` within the outbound timeout. A failure, or the time running out, is
    /// reported to the operator against `key` as [`timed`] words it, and leaves nothing.
    pub(crate) async fn bounded<T, E: Display>(
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

    pub(crate) fn statuses(&self) -> Option<&dyn StatusStore> {
        self.statuses.as_deref()
    }

    /// Keep the status of a run that starts. A failure is reported; the run goes on.
    pub(crate) async fn keep_status(&self, key: &SessionKey, status: &MessageRef) {
        if let Some(statuses) = self.statuses() {
            let act = format!("keep status {}", status.id);
            self.bounded(Some(key), &act, statuses.insert(key, status)).await;
        }
    }

    /// Forget a status about to be replaced. A failure is reported.
    pub(crate) async fn forget_status(&self, key: &SessionKey, status: &MessageRef) {
        if let Some(statuses) = self.statuses() {
            let act = format!("forget status {}", status.id);
            self.bounded(Some(key), &act, statuses.remove(key, status)).await;
        }
    }
}

/// `operation` within `timeout`. The error says what could not be done: `cannot <act>:
/// <error>`, or `cannot <act>: took longer than <n> s`.
pub(crate) async fn timed<T, E: Display>(
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
pub(crate) fn group<T: Eq + Hash>(pairs: Vec<(String, T)>) -> HashMap<String, HashSet<T>> {
    let mut grouped: HashMap<String, HashSet<T>> = HashMap::new();
    for (platform, item) in pairs {
        grouped.entry(platform).or_default().insert(item);
    }
    grouped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(platform: &str, conversation: &str, thread: &str) -> SessionKey {
        SessionKey {
            platform: platform.into(),
            conversation: ConversationId(conversation.into()),
            thread: ThreadId(thread.into()),
        }
    }

    #[test]
    fn a_session_file_has_a_directory_per_platform_and_chat() {
        let path = key("slack", "C0123", "1700000000_1").file_in(Path::new("/data/sessions"));
        assert_eq!(path, Path::new("/data/sessions/slack/C0123/1700000000_1.jsonl"));
    }

    #[test]
    fn ids_cannot_leave_the_session_directory() {
        let path = key("..", "a/b", "1700000000.000100").file_in(Path::new("/s"));
        assert_eq!(path, Path::new("/s/%2E%2E/a%2Fb/1700000000%2E000100.jsonl"));
    }

    #[test]
    fn multibyte_and_empty_ids_are_encoded() {
        let path = key("", "群", "t").file_in(Path::new("/s"));
        assert_eq!(path, Path::new("/s/%/%E7%BE%A4/t.jsonl"));
    }
}
