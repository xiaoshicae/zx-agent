//! Conversations the server starts without a message ([`Trigger`]), and the handle a host asks
//! for them with ([`Triggers`]).

use tokio::sync::mpsc;

use crate::ConversationId;

/// Triggers waiting for the router: a host that sends more waits until it takes one.
const TRIGGER_CAPACITY: usize = 16;

/// A conversation the server starts without a message: a post of `title` in `chat` opens a
/// thread, where `prompt` is asked as `source` asked it. Answers, approval cards and later
/// mentions of the bot in that thread go as in any other.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trigger {
    /// The platform of `chat`, as its channel names itself
    /// ([`ImChannel::platform`](crate::ImChannel::platform)).
    pub platform: String,
    /// A channel or a group the server serves
    /// ([`ServerConfig::conversations`](crate::ServerConfig::conversations)).
    pub chat: ConversationId,
    /// What the post that opens the thread says, such as `Hourly check`.
    pub title: String,
    /// What the model is asked in the thread.
    pub prompt: String,
    /// Who asks, for the operator and the policies: the input's actor, and the name the model
    /// reads before the prompt, such as `schedule:hourly`. Not a user of the platform, whose
    /// name is never looked up.
    pub source: String,
}

/// Asks a server for [`Trigger`]s; every clone asks the same server
/// ([`Server::triggers`](crate::Server::triggers)).
#[derive(Clone)]
pub struct Triggers(mpsc::Sender<Trigger>);

impl Triggers {
    /// A handle, and where what it asks for arrives: the server's router.
    pub(crate) fn channel() -> (Self, mpsc::Receiver<Trigger>) {
        let (sender, receiver) = mpsc::channel(TRIGGER_CAPACITY);
        (Self(sender), receiver)
    }

    /// Ask the server to start `trigger`'s conversation, waiting while 16 triggers wait for it
    /// already; false once the server no longer runs. What the server cannot do with a
    /// trigger, such as start one for a chat it does not serve, it reports to its
    /// [`ServerSink`](crate::ServerSink).
    pub async fn start(&self, trigger: Trigger) -> bool {
        self.0.send(trigger).await.is_ok()
    }
}
