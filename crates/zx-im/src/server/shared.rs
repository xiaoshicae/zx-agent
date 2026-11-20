//! What every conversation shares: the settings, the agent and the sessions it keeps open, the
//! run slots, the statuses, the names of users, and the outbound timeout with its report.

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
use zx_agent::transcript::SessionQuery;
use zx_util::sync::lock;

use super::{Replies, ServerConfig, ServerEvent, ServerSink, SessionKey, StatusStore};
use crate::{ImChannel, ImError, MessageRef, UserId};

/// Settings and resources every conversation reads.
pub(super) struct Shared {
    /// Makes the session of each conversation, and keeps it open while the conversation
    /// holds it.
    pub(super) agent: Agent,
    /// Where new sessions work.
    pub(super) cwd: PathBuf,
    pub(super) sink: Arc<dyn ServerSink>,
    approvers: HashMap<String, HashSet<UserId>>,
    pub(super) replies: Replies,
    pub(super) max_conversations: NonZeroUsize,
    pub(super) max_pending_messages: NonZeroUsize,
    pub(super) max_pending_bytes: NonZeroUsize,
    pub(super) idle: Duration,
    pub(super) outbound_timeout: Duration,
    /// Run slots of all conversations. Tokio's semaphore is fair: it hands permits out in
    /// the order they were requested.
    pub(super) permits: Arc<Semaphore>,
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
    pub(super) fn new(config: ServerConfig, agent: Agent, sink: Arc<dyn ServerSink>) -> Self {
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

    pub(super) fn is_approver(&self, platform: &str, user: &UserId) -> bool {
        self.approvers.get(platform).is_some_and(|users| users.contains(user))
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

/// `pairs` by platform.
pub(super) fn group<T: Eq + Hash>(pairs: Vec<(String, T)>) -> HashMap<String, HashSet<T>> {
    let mut grouped: HashMap<String, HashSet<T>> = HashMap::new();
    for (platform, item) in pairs {
        grouped.entry(platform).or_default().insert(item);
    }
    grouped
}
