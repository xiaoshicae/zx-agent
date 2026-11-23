//! What every conversation shares: the settings, who may use the server, the agents and the
//! sessions they keep open, what chats have of their own, the run slots, the day's usage, the
//! statuses, the names of users, and the outbound timeout with its report.

use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::future::Future;
use std::hash::Hash;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Semaphore;
use zx_agent::Agent;
use zx_agent::ai::Usage;
use zx_agent::transcript::SessionQuery;
use zx_util::sync::lock;

use super::access::Access;
use super::usage::{DayUsage, today};
use super::{ChatConfig, DailyLimit, Replies, ServerConfig, ServerEvent, ServerSink, SessionKey, StatusStore};
use crate::{ConversationId, ImChannel, ImError, MessageRef, UserId};

/// Settings and resources every conversation reads.
pub(super) struct Shared {
    /// Makes the session of each conversation in a chat without an agent of its own, and
    /// keeps it open while the conversation holds it; finds the session of every
    /// conversation, which every agent keeps in its storage.
    agent: Agent,
    /// Where new sessions work.
    pub(super) cwd: PathBuf,
    pub(super) sink: Arc<dyn ServerSink>,
    /// The chats served and the approvers of the chats without their own, which the host may
    /// replace while the server runs.
    access: Access,
    /// What chats have of their own, by platform and chat.
    chats: HashMap<String, HashMap<ConversationId, OwnSettings>>,
    pub(super) replies: Replies,
    pub(super) max_conversations: NonZeroUsize,
    pub(super) max_pending_messages: NonZeroUsize,
    pub(super) max_pending_bytes: NonZeroUsize,
    pub(super) idle: Duration,
    pub(super) long_answer_chars: NonZeroUsize,
    pub(super) outbound_timeout: Duration,
    /// Run slots of all conversations. Tokio's semaphore is fair: it hands permits out in
    /// the order they were requested.
    pub(super) permits: Arc<Semaphore>,
    /// What the work of all chats may use in a day, and what the work of the day used.
    daily: DailyLimit,
    used: Mutex<DayUsage>,
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

/// What a chat has of its own ([`ChatConfig`]), as conversations look it up.
struct OwnSettings {
    agent: Option<Agent>,
    approvers: Option<HashSet<UserId>>,
    daily: DailyLimit,
}

impl Shared {
    /// What the conversations share, from everything in `config` but the lists, which `access`
    /// holds, and the backlog.
    pub(super) fn new(config: ServerConfig, agent: Agent, access: Access, sink: Arc<dyn ServerSink>) -> Self {
        let ServerConfig {
            cwd,
            // Taken into `access` before.
            conversations: _,
            approvers: _,
            chats,
            max_running,
            max_conversations,
            max_pending_messages,
            max_pending_bytes,
            idle,
            long_answer_chars,
            daily,
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
            access,
            chats: own_settings(chats),
            replies,
            idle,
            long_answer_chars,
            outbound_timeout,
            permits: Arc::new(Semaphore::new(max_running.get())),
            daily,
            used: Mutex::default(),
            max_conversations,
            max_pending_messages,
            max_pending_bytes,
            statuses,
            names: Mutex::default(),
            sessions: Mutex::default(),
        }
    }

    /// Find the session of each conversation the storage holds: the most recently started
    /// of those with its labels. Run once, before any message is routed.
    pub(super) async fn index_sessions(&self) -> Result<(), ImError> {
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
    pub(super) fn session_of(&self, key: &SessionKey) -> Option<String> {
        lock(&self.sessions).get(key).cloned()
    }

    /// The conversation `key` goes on in the session `id` from now on: one just made for it.
    pub(super) fn go_on_in(&self, key: &SessionKey, id: &str) {
        lock(&self.sessions).insert(key.clone(), id.to_string());
    }

    /// The name people see for `user` on `channel`, asked once and then remembered. The user's
    /// ID when the platform gives no name; a failed lookup is reported against `key`, once.
    pub(super) async fn name_of(&self, channel: &dyn ImChannel, key: &SessionKey, user: &UserId) -> String {
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

    /// Take `name` as the name of `user` on `platform`, which is then never looked up: the
    /// source of a trigger, which is no user of the platform.
    pub(super) fn name_is(&self, platform: &str, user: &UserId, name: &str) {
        lock(&self.names).insert((platform.to_string(), user.clone()), name.to_string());
    }

    /// The agent whose sessions the conversation `key` is: its chat's own, else the server's.
    pub(super) fn agent_of(&self, key: &SessionKey) -> &Agent {
        match self.own(key).and_then(|own| own.agent.as_ref()) {
            Some(agent) => agent,
            None => &self.agent,
        }
    }

    /// Whether a click by `user` decides approvals in the conversation `key`: whether the
    /// user is among the approvers of its chat, or, for a chat without approvers of its own,
    /// among the server's on its platform.
    pub(super) fn is_approver(&self, key: &SessionKey, user: &UserId) -> bool {
        match self.own(key).and_then(|own| own.approvers.as_ref()) {
            Some(approvers) => approvers.contains(user),
            None => self.access.approves(&key.platform, user),
        }
    }

    /// Whether the server answers in the chat `chat` on `platform`.
    pub(super) fn serves(&self, platform: &str, chat: &ConversationId) -> bool {
        self.access.serves(platform, chat)
    }

    /// What the chat of the conversation `key` has of its own, if anything.
    fn own(&self, key: &SessionKey) -> Option<&OwnSettings> {
        self.own_chat(&key.platform, &key.conversation)
    }

    /// What the chat `chat` on `platform` has of its own, if anything.
    fn own_chat(&self, platform: &str, chat: &ConversationId) -> Option<&OwnSettings> {
        self.chats.get(platform)?.get(chat)
    }

    /// Whether the work of today used up what the work of all chats, or of the chat `chat` on
    /// `platform`, may use in a day: then no work starts there.
    pub(super) fn daily_limit_reached(&self, platform: &str, chat: &ConversationId) -> bool {
        let (all, in_chat) = lock(&self.used).used(today(), platform, chat);
        let own = self.own_chat(platform, chat).map(|own| own.daily);
        self.daily.reached_by(all) || own.is_some_and(|daily| daily.reached_by(in_chat))
    }

    /// Count `usage`, what work in the chat `chat` on `platform` used, as the day's.
    pub(super) fn spend(&self, platform: &str, chat: &ConversationId, usage: &Usage) {
        lock(&self.used).add(today(), platform, chat, usage);
    }

    pub(super) fn failed(&self, key: Option<&SessionKey>, error: String) {
        self.sink.emit(&ServerEvent::Failed {
            key: key.cloned(),
            error,
        });
    }

    /// `operation` within the outbound timeout. A failure, or the time running out, is
    /// reported to the operator against `key` as [`timed`] words it, and leaves nothing.
    pub(super) async fn bounded<T, E: Display>(
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

    pub(super) fn statuses(&self) -> Option<&dyn StatusStore> {
        self.statuses.as_deref()
    }

    /// Keep the status of a run that starts. A failure is reported; the run goes on.
    pub(super) async fn keep_status(&self, key: &SessionKey, status: &MessageRef) {
        if let Some(statuses) = self.statuses() {
            let act = format!("keep status {}", status.id);
            self.bounded(Some(key), &act, statuses.insert(key, status)).await;
        }
    }

    /// Forget a status about to be replaced. A failure is reported.
    pub(super) async fn forget_status(&self, key: &SessionKey, status: &MessageRef) {
        if let Some(statuses) = self.statuses() {
            let act = format!("forget status {}", status.id);
            self.bounded(Some(key), &act, statuses.remove(key, status)).await;
        }
    }
}

/// `operation` within `timeout`. The error says what could not be done: `cannot <act>:
/// <error>`, or `cannot <act>: took longer than <timeout>`.
pub(super) async fn timed<T, E: Display>(
    timeout: Duration,
    act: &str,
    operation: impl Future<Output = Result<T, E>>,
) -> Result<T, String> {
    match tokio::time::timeout(timeout, operation).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(format!("cannot {act}: {error}")),
        Err(_) => Err(format!("cannot {act}: took longer than {timeout:?}")),
    }
}

/// The settings of `chats` by platform and chat, which the server checked to be given once
/// each.
fn own_settings(chats: Vec<ChatConfig>) -> HashMap<String, HashMap<ConversationId, OwnSettings>> {
    let mut grouped: HashMap<String, HashMap<ConversationId, OwnSettings>> = HashMap::new();
    for ChatConfig {
        platform,
        chat,
        agent,
        approvers,
        daily,
    } in chats
    {
        let own = OwnSettings {
            agent,
            approvers: approvers.map(HashSet::from_iter),
            daily,
        };
        grouped.entry(platform).or_default().insert(chat, own);
    }
    grouped
}

/// `pairs` by platform.
pub(super) fn group<T: Eq + Hash>(pairs: Vec<(String, T)>) -> HashMap<String, HashSet<T>> {
    let mut grouped: HashMap<String, HashSet<T>> = HashMap::new();
    for (platform, item) in pairs {
        grouped.entry(platform).or_default().insert(item);
    }
    grouped
}
