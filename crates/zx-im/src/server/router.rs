//! The router: access control, one worker per conversation, replies to chats not served,
//! the conversations that triggers start, the messages an earlier process left pending, and
//! the stop.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::{Id, JoinError, JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

use super::admission::{InputBudget, Limit};
use super::received::Receipts;
use super::trigger::Trigger;
use super::worker::{self, Command, Incoming, Retire, WorkerInput, WorkerSetup};
use super::{ServerEvent, SessionKey, Shared};
use crate::{
    Addressee, Backlog, ChatKind, ConversationId, Destination, ImChannel, ImError, Inbound, Inlet, MessageId,
    MessageKey, MessageRef, Post, ThreadId, UserId,
};

/// Inbound events buffered between the channels and the router, which only passes them on.
const INBOUND_CAPACITY: usize = 64;
/// Input buffered for one conversation. People write slower than a conversation takes input;
/// a full buffer drops the message and reports it.
const INBOX_CAPACITY: usize = 32;
/// Idle notices of conversations, which the router handles at once.
const RETIRE_CAPACITY: usize = 16;
/// Longest command that takes no share of the pending input: a command stays usable even
/// with a tiny text budget, and a long text is not a command anyway.
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

/// Whether `text` is one of the conversations' commands, which take no share of the pending
/// input.
fn is_control(text: &str) -> bool {
    text.len() <= MAX_CONTROL_BYTES && Command::parse(text).is_some()
}

/// Run one channel, which keeps its messages in `backlog`, tagging what it receives with its
/// index for the router.
async fn receive(
    index: usize,
    channel: Arc<dyn ImChannel>,
    backlog: Arc<dyn Backlog>,
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
    let inlet = Inlet::new(channel.platform(), sender, backlog);
    let (result, ()) = tokio::join!(channel.run(inlet, &stop), forward);
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
            if !is_control(&incoming.text) {
                incoming.permit = self.budget.try_reserve(incoming.text.len());
                if incoming.permit.is_none() {
                    return Forward::OverBudget;
                }
            }
        }
        match self.inbox.try_send(input) {
            Ok(()) => {
                self.forwarded += 1;
                Forward::Sent
            }
            Err(mpsc::error::TrySendError::Full(_)) => Forward::Full,
            Err(mpsc::error::TrySendError::Closed(input)) => Forward::Ended(Box::new(input)),
        }
    }
}

/// What became of input passed to a worker.
enum Forward {
    /// The worker has it.
    Sent,
    /// The conversation's budget has no room for the message, so it is dropped.
    OverBudget,
    /// Its inbox is full, so the input is dropped.
    Full,
    /// The worker has ended, after a panic: the input comes back, boxed, as this is rare and
    /// the input large.
    Ended(Box<WorkerInput>),
}

/// The post that opened the thread of a trigger's conversation, on the channel at `index`.
struct Opening {
    index: usize,
    trigger: Trigger,
    root: MessageRef,
}

/// What a router is made of.
pub(super) struct RouterSetup {
    pub(super) channels: Vec<Arc<dyn ImChannel>>,
    /// The chats served, by platform.
    pub(super) allowed: HashMap<String, HashSet<ConversationId>>,
    pub(super) shared: Arc<Shared>,
    pub(super) stop: CancellationToken,
    /// Replaces the statuses an earlier process left behind.
    pub(super) replacing: JoinHandle<()>,
    /// Makes the receipt each message carries until it is handled.
    pub(super) receipts: Receipts,
    /// Where the channels keep their messages.
    pub(super) backlog: Arc<dyn Backlog>,
    /// The triggers a host asks for.
    pub(super) triggered: mpsc::Receiver<Trigger>,
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
    retire_to: mpsc::Sender<Retire>,
    retired: mpsc::Receiver<Retire>,
    workers: HashMap<SessionKey, WorkerHandle>,
    tasks: JoinSet<()>,
    /// The conversation of each task in `tasks`, for the report should it fail.
    conversations: HashMap<Id, SessionKey>,
    /// Replies to chats the server does not serve.
    refusals: JoinSet<()>,
    /// Replaces the statuses an earlier process left behind.
    replacing: JoinHandle<()>,
    /// Makes the receipt each message carries until it is handled.
    receipts: Receipts,
    triggered: mpsc::Receiver<Trigger>,
    /// The posts that open the threads of triggers' conversations.
    openings: JoinSet<Option<Opening>>,
}

impl Router {
    /// Start receiving from the channels of `setup`; nothing is routed before
    /// [`replay`](Self::replay) or [`run`](Self::run).
    pub(super) fn new(setup: RouterSetup) -> Self {
        let RouterSetup {
            channels,
            allowed,
            shared,
            stop,
            replacing,
            receipts,
            backlog,
            triggered,
        } = setup;
        let (tagged, inbound) = mpsc::channel(INBOUND_CAPACITY);
        let mut channel_tasks = JoinSet::new();
        for (index, channel) in channels.iter().enumerate() {
            channel_tasks.spawn(receive(
                index,
                Arc::clone(channel),
                Arc::clone(&backlog),
                tagged.clone(),
                stop.clone(),
            ));
        }
        let (retire_to, retired) = mpsc::channel(RETIRE_CAPACITY);
        Self {
            channels,
            allowed,
            shared,
            stop,
            inbound,
            channel_tasks,
            retire_to,
            retired,
            workers: HashMap::new(),
            tasks: JoinSet::new(),
            conversations: HashMap::new(),
            refusals: JoinSet::new(),
            replacing,
            receipts,
            triggered,
            openings: JoinSet::new(),
        }
    }

    /// Route what an earlier process left pending in the backlog, in the order kept, as if it
    /// arrived now: before anything this process receives. A message from a platform no
    /// channel serves now is reported, and stays pending.
    pub(super) fn replay(&mut self, pending: Vec<(String, Inbound)>) {
        for (platform, event) in pending {
            match self.channels.iter().position(|channel| channel.platform() == platform) {
                Some(index) => self.route(index, event),
                None => self.shared.failed(
                    None,
                    format!("a message from {platform:?} stays in the backlog: no channel is named so"),
                ),
            }
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
                Some(trigger) = self.triggered.recv() => self.trigger(trigger),
                Some(opened) = self.openings.join_next() => self.opened(opened),
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
                images,
            } => {
                // Dropped with the message wherever the message ends here, which handles it.
                let receipt = self.receipts.receipt(MessageKey {
                    platform: platform.clone(),
                    conversation: at.conversation.clone(),
                    message: id.clone(),
                });
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
                        images,
                        permit: None,
                        receipt,
                    };
                    self.message(channel, key, incoming);
                } else if addressee == Addressee::Bot {
                    // Only a message meant for the bot hears why it gets no answer; the
                    // rest of a group's talk is none of its business.
                    self.refuse(channel, key, sender);
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
        if incoming.text.len() > self.shared.max_pending_bytes.get() && !is_control(&incoming.text) {
            return self.over_capacity(channel, &key, addressee, Limit::MessageBytes);
        }
        let input = WorkerInput::Message(Box::new(incoming));
        let input = match self.workers.get_mut(&key) {
            Some(worker) => match worker.forward(input) {
                Forward::Sent => return,
                Forward::OverBudget => return self.over_capacity(channel, &key, addressee, Limit::Pending),
                Forward::Full => return self.over_capacity(channel, &key, addressee, Limit::Inbox),
                // A new worker takes over.
                Forward::Ended(input) => *input,
            },
            None => input,
        };
        if self.tasks.len() >= self.shared.max_conversations.get() {
            return self.over_capacity(channel, &key, addressee, Limit::Conversations);
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
            retire_to: self.retire_to.clone(),
            stop: self.stop.clone(),
        }));
        self.conversations.insert(task.id(), key.clone());
        let mut worker = WorkerHandle {
            inbox,
            budget,
            forwarded: 0,
        };
        // A new inbox has room for one message, and a new budget for one that is not longer
        // than it all, which was refused above.
        if !matches!(worker.forward(input), Forward::Sent) {
            self.shared.failed(
                Some(&key),
                "a message was dropped: the conversation started for it did not take it".into(),
            );
        }
        self.workers.insert(key, worker);
    }

    /// Pass a click to the conversation's worker. Without a worker nothing is pending:
    /// approvals end with their run, and the router ends only idle conversations.
    fn decision(&mut self, key: &SessionKey, input: WorkerInput) {
        let full = match self.workers.get_mut(key) {
            Some(worker) => match worker.forward(input) {
                Forward::Full => true,
                // A click takes no share of the budget, and a worker that has ended took its
                // approvals with it.
                Forward::Sent | Forward::OverBudget | Forward::Ended(_) => false,
            },
            None => false,
        };
        if full {
            self.shared.failed(
                Some(key),
                "an approval click was dropped: the conversation's inbox is full".into(),
            );
        }
    }

    /// Start `trigger`'s conversation without holding up the router: post its title in its
    /// chat, and once that is posted ([`Self::opened`]) ask its prompt in the thread the post
    /// opens. A trigger for a platform no channel serves, for a chat the server does not
    /// serve, beyond `max_conversations`, or once the day's usage there is used up, is
    /// reported, and nothing is posted.
    fn trigger(&mut self, trigger: Trigger) {
        let source = &trigger.source;
        let Some(index) = self
            .channels
            .iter()
            .position(|channel| channel.platform() == trigger.platform)
        else {
            let error = format!(
                "a trigger of {source} names {:?}, which no channel serves",
                trigger.platform
            );
            return self.shared.failed(None, error);
        };
        let served = self
            .allowed
            .get(&trigger.platform)
            .is_some_and(|chats| chats.contains(&trigger.chat));
        if !served {
            let error = format!("a trigger of {source} names chat {}, which is not served", trigger.chat);
            return self.shared.failed(None, error);
        }
        // Each opening becomes a conversation.
        if self.tasks.len() + self.openings.len() >= self.shared.max_conversations.get() {
            let error = format!("a trigger of {source} was not started: max_conversations conversations are served");
            return self.shared.failed(None, error);
        }
        if self.shared.daily_limit_reached(&trigger.platform, &trigger.chat) {
            let error = format!("a trigger of {source} was not started: the day's usage limit is reached");
            return self.shared.failed(None, error);
        }
        let channel = Arc::clone(&self.channels[index]);
        let shared = Arc::clone(&self.shared);
        self.openings.spawn(async move {
            let to = Destination {
                conversation: trigger.chat.clone(),
                thread: None,
            };
            let act = format!("open a thread in {} for {}", trigger.chat, trigger.source);
            let title = Post::Text(trigger.title.clone());
            // A failure was reported; the trigger is dropped.
            let root = shared.bounded(None, &act, channel.send(&to, &title)).await?;
            Some(Opening { index, trigger, root })
        });
    }

    /// Ask the prompt of a trigger in the thread its post opened, as a mention of the bot
    /// there by its source: from now on a conversation like any other. A post that failed was
    /// reported.
    fn opened(&mut self, opened: Result<Option<Opening>, JoinError>) {
        let Opening { index, trigger, root } = match opened {
            Ok(Some(opening)) => opening,
            Ok(None) => return,
            Err(error) => {
                return self
                    .shared
                    .failed(None, format!("opening the thread of a trigger failed: {error}"));
            }
        };
        let channel = Arc::clone(&self.channels[index]);
        let key = SessionKey {
            platform: trigger.platform,
            conversation: trigger.chat,
            thread: Some(ThreadId(root.id.0.clone())),
        };
        let source = UserId(trigger.source.clone());
        self.shared.name_is(&key.platform, &source, &trigger.source);
        self.shared.sink.emit(&ServerEvent::Triggered {
            key: key.clone(),
            source: trigger.source,
        });
        let incoming = Incoming {
            // No message of the platform's: none is acknowledged, and none has images.
            message: MessageRef {
                at: key.destination(),
                id: MessageId(format!("{}:trigger", root.id)),
            },
            sender: source,
            addressee: Addressee::Bot,
            text: trigger.prompt,
            images: Vec::new(),
            permit: None,
            receipt: self.receipts.unkept(),
        };
        self.message(channel, key, incoming);
    }

    /// Answer a chat the server does not serve, without holding up the router, and tell the
    /// operator where the message came from and who wrote it.
    fn refuse(&mut self, channel: Arc<dyn ImChannel>, key: SessionKey, sender: UserId) {
        self.reply(channel, key.destination(), self.shared.replies.unauthorized.clone());
        self.shared.sink.emit(&ServerEvent::Unauthorized { key, user: sender });
    }

    /// Refuse a message that met `limit`. The operator hears of every one, and of the limit;
    /// the conversation only of one meant for the bot, so that the bot never speaks up in
    /// talk among people.
    fn over_capacity(&mut self, channel: Arc<dyn ImChannel>, key: &SessionKey, addressee: Addressee, limit: Limit) {
        self.shared.failed(Some(key), limit.refusal().into());
        if addressee == Addressee::Bot {
            self.reply(channel, key.destination(), self.shared.replies.overloaded.clone());
        }
    }

    fn reply(&mut self, channel: Arc<dyn ImChannel>, to: Destination, text: String) {
        // Even rejection replies have a bound, so a busy platform cannot accumulate tasks.
        if self.refusals.len() >= self.shared.max_conversations.get() {
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
        // A trigger not started yet is lost with the stop: the host's next ones fail at once,
        // and a thread being opened is given up.
        self.triggered.close();
        self.openings.shutdown().await;
        // A closed receiver makes waiting senders fail at once, so no worker can hold up the
        // stop. The channels are read to the end instead.
        self.retired.close();
        // What the channels pass on during the stop is not routed: each message stays in the
        // backlog, for the next start, and a click finds its approval ended. The channels
        // stop sending once cancelled, and their tasks then drop the senders, which ends this
        // loop; reading on means no channel waits on a full buffer meanwhile.
        while self.inbound.recv().await.is_some() {}
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
