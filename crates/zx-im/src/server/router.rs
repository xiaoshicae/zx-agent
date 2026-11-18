//! The router: access control, one worker per conversation, replies to chats not served,
//! and the stop.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::{Id, JoinError, JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::{
    Addressee, ChatKind, ConversationId, Destination, ImChannel, ImError, Inbound, MessageId, MessageRef, Post,
    ThreadId,
};

use super::admission::InputBudget;
use super::worker::{self, Incoming, Retire, WorkerInput, WorkerSetup};
use super::{SessionKey, Shared};

/// Inbound events buffered between the channels and the router, which only passes them on.
const INBOUND_CAPACITY: usize = 64;
/// Input buffered for one conversation. People write slower than a conversation takes input;
/// a full buffer drops the message and reports it.
const INBOX_CAPACITY: usize = 32;
/// Idle notices of conversations, which the router handles at once.
const RETIRE_CAPACITY: usize = 16;
/// Short control messages remain usable even with a tiny configured text budget.
const MAX_CONTROL_BYTES: usize = 64;

/// The conversation a message belongs to: the direct chat, or the thread in a group. A
/// mention posted in a group itself starts a thread under it; anything else posted there
/// belongs to no conversation, `None`.
fn conversation(
    platform: String,
    chat: ChatKind,
    addressee: Addressee,
    id: &MessageId,
    at: &Destination,
) -> Option<SessionKey> {
    let thread = match (chat, &at.thread) {
        (ChatKind::Direct, _) => None,
        (ChatKind::Group, Some(thread)) => Some(thread.clone()),
        (ChatKind::Group, None) if addressee == Addressee::Bot => Some(ThreadId(id.0.clone())),
        (ChatKind::Group, None) => return None,
    };
    Some(SessionKey {
        platform,
        conversation: at.conversation.clone(),
        thread,
    })
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
    budget: Arc<InputBudget>,
    /// Input passed to the worker so far, compared with [`Retire::received`].
    forwarded: u64,
}

impl WorkerHandle {
    /// Pass `input` to the worker, counting what it takes, as the retire handshake needs
    /// every input counted.
    fn forward(&mut self, mut input: WorkerInput) -> Forward {
        if let WorkerInput::Message(incoming) = &mut input {
            // A closed worker's input may be handed to a new worker with a new budget.
            incoming.permit = None;
            let control = incoming.text.len() <= MAX_CONTROL_BYTES && worker::is_command(&incoming.text);
            if !control {
                incoming.permit = self.budget.try_reserve(incoming.text.len());
                if incoming.permit.is_none() {
                    return Forward::Full;
                }
            }
        }
        match self.inbox.try_send(input) {
            Ok(()) => {
                self.forwarded += 1;
                Forward::Sent
            }
            Err(mpsc::error::TrySendError::Full(_)) => Forward::Full,
            Err(mpsc::error::TrySendError::Closed(input)) => Forward::Ended(input),
        }
    }
}

/// What became of input passed to a worker.
enum Forward {
    /// The worker has it.
    Sent,
    /// Its inbox is full, so the input is dropped.
    Full,
    /// The worker has ended, after a panic: the input comes back.
    Ended(WorkerInput),
}

/// Passes what the channels receive to the conversations' workers, and ends them.
pub(super) struct Router {
    channels: Vec<Arc<dyn ImChannel>>,
    allowed: HashMap<String, HashSet<ConversationId>>,
    shared: Arc<Shared>,
    stop: CancellationToken,
    /// What the channels receive, tagged with the channel's index.
    inbound: mpsc::Receiver<(usize, Inbound)>,
    channel_tasks: JoinSet<(usize, Result<(), ImError>)>,
    retire: mpsc::Sender<Retire>,
    retired: mpsc::Receiver<Retire>,
    workers: HashMap<SessionKey, WorkerHandle>,
    tasks: JoinSet<()>,
    /// The conversation of each task in `tasks`, for the report should it fail.
    conversations: HashMap<Id, SessionKey>,
    /// Replies to chats the server does not serve.
    refusals: JoinSet<()>,
    /// Replaces the statuses an earlier process left behind.
    replacing: JoinHandle<()>,
}

impl Router {
    /// Start receiving from `channels`; nothing is routed before [`run`](Self::run).
    pub(super) fn new(
        channels: Vec<Arc<dyn ImChannel>>,
        allowed: HashMap<String, HashSet<ConversationId>>,
        shared: Arc<Shared>,
        stop: CancellationToken,
        replacing: JoinHandle<()>,
    ) -> Self {
        let (tagged, inbound) = mpsc::channel(INBOUND_CAPACITY);
        let mut channel_tasks = JoinSet::new();
        for (index, channel) in channels.iter().enumerate() {
            channel_tasks.spawn(receive(index, Arc::clone(channel), tagged.clone(), stop.clone()));
        }
        let (retire, retired) = mpsc::channel(RETIRE_CAPACITY);
        Self {
            channels,
            allowed,
            shared,
            stop,
            inbound,
            channel_tasks,
            retire,
            retired,
            workers: HashMap::new(),
            tasks: JoinSet::new(),
            conversations: HashMap::new(),
            refusals: JoinSet::new(),
            replacing,
        }
    }

    /// Route until the stop or a channel's failure, then end every conversation.
    pub(super) async fn run(mut self) -> Result<(), ImError> {
        let result = self.serve().await;
        self.shutdown().await;
        result
    }

    async fn serve(&mut self) -> Result<(), ImError> {
        loop {
            tokio::select! {
                () = self.stop.cancelled() => return Ok(()),
                Some((index, event)) = self.inbound.recv() => self.route(index, event),
                Some(retire) = self.retired.recv() => self.retire(&retire),
                Some(ended) = self.channel_tasks.join_next() => self.channel_ended(ended)?,
                Some(ended) = self.tasks.join_next_with_id() => self.note_ended("conversation", ended),
                Some(ended) = self.refusals.join_next_with_id() => self.note_ended("reply", ended),
            }
        }
    }

    /// A channel's task ended: an error unless the server is stopping.
    fn channel_ended(&self, ended: Result<(usize, Result<(), ImError>), JoinError>) -> Result<(), ImError> {
        let (index, result) = ended.map_err(|error| ImError::Transport(format!("a channel task failed: {error}")))?;
        result?;
        if self.stop.is_cancelled() {
            return Ok(());
        }
        Err(ImError::Transport(format!(
            "the {} channel stopped while the server was running",
            self.channels[index].platform()
        )))
    }

    /// Forget an ended task of `what` kind; a failure is reported against its conversation,
    /// when it had one.
    fn note_ended(&mut self, what: &str, ended: Result<(Id, ()), JoinError>) {
        let (id, failure) = match ended {
            Ok((id, ())) => (id, None),
            Err(error) => (error.id(), Some(error)),
        };
        let key = self.conversations.remove(&id);
        // A failed worker closes its inbox without the retire handshake. Do not retain
        // its handle, or remove a replacement worker whose inbox is still open.
        if let Some(key) = &key
            && self.workers.get(key).is_some_and(|worker| worker.inbox.is_closed())
        {
            self.workers.remove(key);
        }
        if let Some(error) = failure {
            self.shared
                .failed(key.as_ref(), format!("a {what} task failed: {error}"));
        }
    }

    fn route(&mut self, index: usize, event: Inbound) {
        let channel = Arc::clone(&self.channels[index]);
        let platform = channel.platform().to_string();
        match event {
            Inbound::Message {
                id,
                at,
                chat,
                addressee,
                sender,
                text,
            } => {
                let Some(key) = conversation(platform, chat, addressee, &id, &at) else {
                    return;
                };
                let served = self
                    .allowed
                    .get(&key.platform)
                    .is_some_and(|chats| chats.contains(&key.conversation));
                if served {
                    let incoming = Incoming {
                        message: MessageRef { at, id },
                        sender,
                        addressee,
                        text,
                        permit: None,
                    };
                    self.message(channel, key, incoming);
                } else if addressee == Addressee::Bot {
                    // Only a message meant for the bot hears why it gets no answer; the
                    // rest of a group's talk is none of its business.
                    self.refuse(channel, key.destination());
                }
            }
            Inbound::Decision {
                at,
                user,
                approval,
                choice,
            } => {
                // Cards are posted where the conversation posts: its thread, or the direct
                // chat itself.
                let key = SessionKey {
                    platform,
                    conversation: at.conversation,
                    thread: at.thread,
                };
                self.decision(&key, WorkerInput::Decision { user, approval, choice });
            }
        }
    }

    /// Pass a message to the conversation's worker, starting one when there is none: for a
    /// message meant for the bot, or one in a conversation the bot took part in, which has a
    /// session.
    fn message(&mut self, channel: Arc<dyn ImChannel>, key: SessionKey, incoming: Incoming) {
        let addressee = incoming.addressee;
        // Talk in a thread the bot never joined is not a conversation with it.
        if addressee != Addressee::Bot && !self.workers.contains_key(&key) && self.shared.session_of(&key).is_none() {
            return;
        }
        let control = incoming.text.len() <= MAX_CONTROL_BYTES && worker::is_command(&incoming.text);
        if incoming.text.len() > self.shared.max_pending_bytes.get() && !control {
            self.over_capacity(channel, &key, addressee);
            return;
        }
        let input = WorkerInput::Message(incoming);
        let input = match self.workers.get_mut(&key) {
            Some(worker) => match worker.forward(input) {
                Forward::Sent => return,
                Forward::Full => {
                    self.over_capacity(channel, &key, addressee);
                    return;
                }
                // A new worker takes over.
                Forward::Ended(input) => input,
            },
            None => input,
        };
        if self.tasks.len() >= self.shared.max_conversations {
            self.over_capacity(channel, &key, addressee);
            return;
        }
        let (inbox, receiver) = mpsc::channel(INBOX_CAPACITY);
        let budget = Arc::new(InputBudget::new(
            self.shared.max_pending_messages,
            self.shared.max_pending_bytes,
        ));
        let task = self.tasks.spawn(worker::run(WorkerSetup {
            key: key.clone(),
            channel,
            shared: Arc::clone(&self.shared),
            inbox: receiver,
            budget: Arc::clone(&budget),
            retire: self.retire.clone(),
            stop: self.stop.clone(),
        }));
        self.conversations.insert(task.id(), key.clone());
        let mut worker = WorkerHandle {
            inbox,
            budget,
            forwarded: 0,
        };
        // A new inbox has room for one message.
        if !matches!(worker.forward(input), Forward::Sent) {
            self.dropped(&key);
        }
        self.workers.insert(key, worker);
    }

    /// Pass a click to the conversation's worker. Without a worker nothing is pending:
    /// approvals end with their run, and the router ends only idle conversations.
    fn decision(&mut self, key: &SessionKey, input: WorkerInput) {
        let full = match self.workers.get_mut(key) {
            Some(worker) => match worker.forward(input) {
                Forward::Full => true,
                // A worker that has ended took its approvals with it.
                Forward::Sent | Forward::Ended(_) => false,
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
        self.reply(channel, to, self.shared.replies.unauthorized.clone());
    }

    /// Refuse a message for want of capacity. The operator hears of every one; the
    /// conversation only of one meant for the bot, so that the bot never speaks up in talk
    /// among people.
    fn over_capacity(&mut self, channel: Arc<dyn ImChannel>, key: &SessionKey, addressee: Addressee) {
        self.shared.failed(
            Some(key),
            "conversation capacity exceeded; message was not accepted".into(),
        );
        if addressee == Addressee::Bot {
            self.reply(channel, key.destination(), self.shared.replies.overloaded.clone());
        }
    }

    fn reply(&mut self, channel: Arc<dyn ImChannel>, to: Destination, text: String) {
        // Even rejection replies have a bound, so a busy platform cannot accumulate tasks.
        if self.refusals.len() >= self.shared.max_conversations {
            return;
        }
        let shared = Arc::clone(&self.shared);
        self.refusals.spawn(async move {
            let text = Post::Text(text);
            let act = format!("answer in {}", to.conversation);
            shared.bounded(None, &act, channel.send(&to, &text)).await;
        });
    }

    fn retire(&mut self, retire: &Retire) {
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

    async fn shutdown(mut self) {
        self.stop.cancel();
        // A closed receiver makes waiting senders fail at once, so no worker can hold up the
        // stop. The channels are read to the end instead.
        self.retired.close();
        // The platforms were told about these events already and will not send them again.
        // Routed as usual, each message reaches a stopping worker, which answers that it will
        // not run it. The channels stop sending once cancelled, and their tasks then drop
        // the senders, which ends this loop; reading on means no channel waits on a full
        // buffer meanwhile.
        while let Some((index, event)) = self.inbound.recv().await {
            self.route(index, event);
        }
        // Without their inboxes, workers end once their runs have stopped and they have
        // posted.
        self.workers.clear();
        while let Some(ended) = self.tasks.join_next_with_id().await {
            self.note_ended("conversation", ended);
        }
        // A channel may report an error while its connection closes; the server is stopping
        // either way.
        while self.channel_tasks.join_next().await.is_some() {}
        while let Some(ended) = self.refusals.join_next_with_id().await {
            self.note_ended("reply", ended);
        }
        // It gives up at the stop, which was triggered above.
        if let Err(error) = self.replacing.await {
            self.shared.failed(
                None,
                format!("replacing the statuses an earlier process left failed: {error}"),
            );
        }
    }
}
