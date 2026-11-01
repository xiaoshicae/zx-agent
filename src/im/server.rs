//! The router: access control, one worker per thread, the shared run limit and the stop.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::AgentSession;
use crate::agent::ApprovalResult;
use crate::interaction::Interaction;

use super::worker::{self, Retire, WorkerInput, WorkerSetup};
use super::{ApprovalId, ConversationId, Destination, ImChannel, ImError, Inbound, Outbound, ThreadId, UserId};

/// Inbound events buffered between the channels and the router, which only passes them on.
const INBOUND_CAPACITY: usize = 64;
/// Input buffered for one conversation. People write slower than a conversation takes input;
/// a full buffer drops the message and reports it.
const INBOX_CAPACITY: usize = 32;
/// Idle notices of conversations, which the router handles at once.
const RETIRE_CAPACITY: usize = 16;

/// Defaults from docs/serve-im-review.md, section 3.6.
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

fn segment(id: &str) -> String {
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
    /// An approval ended: who decided what (docs/serve-im-review.md, gap 4).
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

/// Texts the server posts besides the model's answers. The defaults are Chinese, as in
/// docs/serve-im-review.md.
#[derive(Clone, Debug)]
pub struct Replies {
    /// Posted when a run starts; replaced by the answer when it ends.
    pub working: String,
    /// Posted when a conversation waits for a free run slot.
    pub queued: String,
    /// The only answer in a chat the server does not serve.
    pub unauthorized: String,
    /// The answer to a click by someone who is not an approver.
    pub not_approver: String,
    /// A cancelled run, and the answer to `/cancel` while waiting for a slot.
    pub cancelled: String,
    /// The answer to `/cancel` when nothing runs.
    pub nothing_to_cancel: String,
    /// A run that completed without text.
    pub no_answer: String,
    /// Appended to an answer cut off at the output limit.
    pub truncated: String,
    /// Appended when the turn budget ran out.
    pub max_turns: String,
    /// A run that reached its time limit.
    pub timed_out: String,
    /// Followed by the error of a failed run.
    pub failed: String,
    /// Followed by the error of a session that could not be opened.
    pub cannot_open: String,
}

impl Default for Replies {
    fn default() -> Self {
        Self {
            working: "处理中…".into(),
            queued: "排队中：同时运行的任务已满，轮到时开始处理。".into(),
            unauthorized: "未授权：本聊天不在服务名单中。".into(),
            not_approver: "只有审批人名单中的成员可以批准或拒绝，本次点击不生效。".into(),
            cancelled: "已取消。".into(),
            nothing_to_cancel: "没有进行中的任务。".into(),
            no_answer: "（没有回答）".into(),
            truncated: "（回答达到输出上限，已截断）".into(),
            max_turns: "（已达到轮次上限，任务没有完成）".into(),
            timed_out: "运行超时，任务没有完成。".into(),
            failed: "运行失败：".into(),
            cannot_open: "无法打开会话：".into(),
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
    /// Limit for each send or update; a call that takes longer counts as failed.
    pub outbound_timeout: Duration,
    pub replies: Replies,
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
    pub fn new(config: ServerConfig, factory: Arc<dyn SessionFactory>, sink: Arc<dyn ServerSink>) -> Self {
        Self { config, factory, sink }
    }

    /// Serve `channels` until `cancel` is triggered or a channel fails. Stopping cancels the
    /// runs and waits until each conversation has posted how it ended; a channel's error is
    /// then returned. Two channels with the same platform name are a configuration error.
    pub async fn run(self, channels: Vec<Arc<dyn ImChannel>>, cancel: &CancellationToken) -> Result<(), ImError> {
        let mut platforms = HashSet::new();
        for channel in &channels {
            if !platforms.insert(channel.platform()) {
                return Err(ImError::Config(format!(
                    "more than one channel is named {:?}",
                    channel.platform()
                )));
            }
        }
        let ServerConfig {
            conversations,
            approvers,
            max_running,
            idle,
            outbound_timeout,
            replies,
        } = self.config;
        let shared = Arc::new(Shared {
            factory: self.factory,
            sink: self.sink,
            approvers: group(approvers),
            replies,
            idle,
            outbound_timeout,
            permits: Arc::new(Semaphore::new(max_running.get())),
        });
        let stop = cancel.child_token();
        let (tagged, mut inbound) = mpsc::channel(INBOUND_CAPACITY);
        let mut channel_tasks = JoinSet::new();
        for (index, channel) in channels.iter().enumerate() {
            channel_tasks.spawn(receive(index, Arc::clone(channel), tagged.clone(), stop.clone()));
        }
        drop(tagged);
        let (retire, mut retired) = mpsc::channel(RETIRE_CAPACITY);
        let mut router = Router {
            channels,
            allowed: group(conversations),
            shared,
            stop,
            retire,
            workers: HashMap::new(),
            tasks: JoinSet::new(),
            refusals: JoinSet::new(),
        };
        let result = router.serve(&mut inbound, &mut retired, &mut channel_tasks).await;
        // Closed receivers make waiting senders fail at once, so neither a channel nor a
        // worker can hold up the stop.
        drop(inbound);
        drop(retired);
        router.shutdown(channel_tasks).await;
        result
    }
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
}

impl Shared {
    pub(crate) fn is_approver(&self, platform: &str, user: &UserId) -> bool {
        self.approvers.get(platform).is_some_and(|users| users.contains(user))
    }

    pub(crate) fn failed(&self, key: Option<&SessionKey>, error: String) {
        self.sink.emit(&ServerEvent::Failed {
            key: key.cloned(),
            error,
        });
    }
}

fn group<T: Eq + Hash>(pairs: Vec<(String, T)>) -> HashMap<String, HashSet<T>> {
    let mut grouped: HashMap<String, HashSet<T>> = HashMap::new();
    for (platform, item) in pairs {
        grouped.entry(platform).or_default().insert(item);
    }
    grouped
}

/// Run one channel, tagging what it receives with its index for the router.
async fn receive(
    index: usize,
    channel: Arc<dyn ImChannel>,
    tagged: mpsc::Sender<(usize, Inbound)>,
    stop: CancellationToken,
) -> (usize, Result<(), ImError>) {
    let (sender, mut receiver) = mpsc::channel(INBOUND_CAPACITY);
    let forward = async move {
        while let Some(event) = receiver.recv().await {
            if tagged.send((index, event)).await.is_err() {
                // The router no longer reads: the server is stopping.
                break;
            }
        }
    };
    let (result, ()) = tokio::join!(channel.run(sender, &stop), forward);
    (index, result)
}

struct WorkerHandle {
    inbox: mpsc::Sender<WorkerInput>,
    /// Input passed to the worker so far, compared with [`Retire::received`].
    forwarded: u64,
}

struct Router {
    channels: Vec<Arc<dyn ImChannel>>,
    allowed: HashMap<String, HashSet<ConversationId>>,
    shared: Arc<Shared>,
    stop: CancellationToken,
    retire: mpsc::Sender<Retire>,
    workers: HashMap<SessionKey, WorkerHandle>,
    tasks: JoinSet<()>,
    /// Replies to chats the server does not serve.
    refusals: JoinSet<()>,
}

impl Router {
    async fn serve(
        &mut self,
        inbound: &mut mpsc::Receiver<(usize, Inbound)>,
        retired: &mut mpsc::Receiver<Retire>,
        channel_tasks: &mut JoinSet<(usize, Result<(), ImError>)>,
    ) -> Result<(), ImError> {
        loop {
            tokio::select! {
                () = self.stop.cancelled() => return Ok(()),
                Some((index, event)) = inbound.recv() => self.route(index, event),
                Some(retire) = retired.recv() => self.retire(retire),
                Some(ended) = channel_tasks.join_next() => {
                    let (index, result) =
                        ended.map_err(|error| ImError::Transport(format!("a channel task failed: {error}")))?;
                    result?;
                    if !self.stop.is_cancelled() {
                        return Err(ImError::Transport(format!(
                            "the {} channel stopped while the server was running",
                            self.channels[index].platform()
                        )));
                    }
                }
                Some(ended) = self.tasks.join_next() => {
                    if let Err(error) = ended {
                        self.shared.failed(None, format!("a conversation task failed: {error}"));
                    }
                }
                Some(ended) = self.refusals.join_next() => {
                    if let Err(error) = ended {
                        self.shared.failed(None, format!("a reply task failed: {error}"));
                    }
                }
            }
        }
    }

    fn route(&mut self, index: usize, event: Inbound) {
        let channel = Arc::clone(&self.channels[index]);
        let platform = channel.platform().to_string();
        match event {
            Inbound::Message {
                id,
                at,
                sender: _,
                text,
            } => {
                // A message posted in the chat itself starts a thread under it.
                let thread = at.thread.unwrap_or(ThreadId(id.0));
                let key = SessionKey {
                    platform,
                    conversation: at.conversation,
                    thread,
                };
                let served = self
                    .allowed
                    .get(&key.platform)
                    .is_some_and(|chats| chats.contains(&key.conversation));
                if served {
                    self.message(channel, key, text);
                } else {
                    self.refuse(channel, key.destination());
                }
            }
            Inbound::Decision {
                at,
                user,
                approval,
                choice,
            } => {
                // Cards are posted in a conversation's thread; a click elsewhere is not ours.
                let Some(thread) = at.thread else {
                    return;
                };
                let key = SessionKey {
                    platform,
                    conversation: at.conversation,
                    thread,
                };
                self.decision(&key, WorkerInput::Decision { user, approval, choice });
            }
        }
    }

    /// Pass a message to the conversation's worker, starting one when there is none.
    fn message(&mut self, channel: Arc<dyn ImChannel>, key: SessionKey, text: String) {
        let input = WorkerInput::Message { text };
        let input = match self.workers.get_mut(&key) {
            Some(worker) => match worker.inbox.try_send(input) {
                Ok(()) => {
                    worker.forwarded += 1;
                    return;
                }
                Err(mpsc::error::TrySendError::Full(_)) => None,
                // The worker has ended, after a panic: a new one takes over.
                Err(mpsc::error::TrySendError::Closed(input)) => Some(input),
            },
            None => Some(input),
        };
        let Some(input) = input else {
            self.dropped(&key);
            return;
        };
        let (inbox, receiver) = mpsc::channel(INBOX_CAPACITY);
        self.tasks.spawn(worker::run(WorkerSetup {
            key: key.clone(),
            channel,
            shared: Arc::clone(&self.shared),
            inbox: receiver,
            retire: self.retire.clone(),
            stop: self.stop.clone(),
        }));
        let mut worker = WorkerHandle { inbox, forwarded: 0 };
        // A new inbox has room for one message.
        match worker.inbox.try_send(input) {
            Ok(()) => worker.forwarded += 1,
            Err(_) => self.dropped(&key),
        }
        self.workers.insert(key, worker);
    }

    /// Pass a click to the conversation's worker. Without a worker nothing is pending:
    /// approvals end with their run, and the router ends only idle conversations.
    fn decision(&mut self, key: &SessionKey, input: WorkerInput) {
        let full = match self.workers.get_mut(key) {
            Some(worker) => match worker.inbox.try_send(input) {
                Ok(()) => {
                    worker.forwarded += 1;
                    false
                }
                Err(mpsc::error::TrySendError::Full(_)) => true,
                // The worker has ended, and its approvals with it.
                Err(mpsc::error::TrySendError::Closed(_)) => false,
            },
            None => false,
        };
        if full {
            self.dropped(key);
        }
    }

    fn dropped(&self, key: &SessionKey) {
        self.shared.failed(
            Some(key),
            "too many messages wait in this conversation; one was dropped".into(),
        );
    }

    /// Answer a chat the server does not serve, without holding up the router.
    fn refuse(&mut self, channel: Arc<dyn ImChannel>, to: Destination) {
        let shared = Arc::clone(&self.shared);
        self.refusals.spawn(async move {
            let text = Outbound::Text(shared.replies.unauthorized.clone());
            match tokio::time::timeout(shared.outbound_timeout, channel.send(&to, &text)).await {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => shared.failed(None, format!("cannot answer in {}: {error}", to.conversation)),
                Err(_) => shared.failed(
                    None,
                    format!(
                        "answering in {} took longer than {} s",
                        to.conversation,
                        shared.outbound_timeout.as_secs()
                    ),
                ),
            }
        });
    }

    fn retire(&mut self, retire: Retire) {
        // Equal counts mean the worker has taken everything sent to it and holds no session:
        // without its inbox it ends. Otherwise a message is on its way and the worker goes on.
        if self
            .workers
            .get(&retire.key)
            .is_some_and(|worker| worker.forwarded == retire.received)
        {
            self.workers.remove(&retire.key);
        }
    }

    async fn shutdown(mut self, mut channel_tasks: JoinSet<(usize, Result<(), ImError>)>) {
        self.stop.cancel();
        // Without their inboxes, workers end once their runs have stopped and posted.
        self.workers.clear();
        while let Some(ended) = self.tasks.join_next().await {
            if let Err(error) = ended {
                self.shared.failed(None, format!("a conversation task failed: {error}"));
            }
        }
        // A channel may report an error while its connection closes; the server is stopping
        // either way.
        while channel_tasks.join_next().await.is_some() {}
        while let Some(ended) = self.refusals.join_next().await {
            if let Err(error) = ended {
                self.shared.failed(None, format!("a reply task failed: {error}"));
            }
        }
    }
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
