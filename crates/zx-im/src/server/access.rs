//! Who may use the server: the chats it answers in and the users whose clicks decide
//! approvals, which the host may replace while the server runs ([`Access`]).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use zx_util::sync::lock;

use super::shared::group;
use crate::{ConversationId, UserId};

/// The lists, by platform, as the server looks them up.
struct Lists {
    conversations: HashMap<String, HashSet<ConversationId>>,
    approvers: HashMap<String, HashSet<UserId>>,
}

/// Where the host replaces who may use a server, before it runs and while it does
/// ([`Server::access`]): the chats it answers in, and the users whose clicks decide approvals
/// in the chats without approvers of their own. Its clones share the lists.
///
/// [`Server::access`]: super::Server::access
#[derive(Clone)]
pub struct Access(Arc<Mutex<Lists>>);

impl Access {
    pub(super) fn new(conversations: Vec<(String, ConversationId)>, approvers: Vec<(String, UserId)>) -> Self {
        Self(Arc::new(Mutex::new(Lists {
            conversations: group(conversations),
            approvers: group(approvers),
        })))
    }

    /// Answer in `conversations` and take the clicks of `approvers` from now on, in place of
    /// the lists so far, given as [`ServerConfig::conversations`] and
    /// [`ServerConfig::approvers`] are. Work that runs goes on. A message from a chat no longer
    /// served gets [`Replies::unauthorized`], and a click by a user no longer listed decides
    /// nothing. What chats have of their own ([`ServerConfig::chats`]) stays as the server was
    /// made with it, and is not checked against the new lists: the settings of a chat no
    /// longer served go unused until a later list serves it again. A chat that a new list
    /// starts to serve is served with those settings, or as a chat without any: a host whose
    /// configuration gives it others leaves it off the list until it makes the server again,
    /// as the program `zx-im` does.
    ///
    /// [`ServerConfig::conversations`]: super::ServerConfig::conversations
    /// [`ServerConfig::approvers`]: super::ServerConfig::approvers
    /// [`ServerConfig::chats`]: super::ServerConfig::chats
    /// [`Replies::unauthorized`]: super::Replies::unauthorized
    pub fn replace(&self, conversations: Vec<(String, ConversationId)>, approvers: Vec<(String, UserId)>) {
        let lists = Lists {
            conversations: group(conversations),
            approvers: group(approvers),
        };
        *lock(&self.0) = lists;
    }

    /// Whether the server answers in the chat `chat` on `platform`. The program asks as well,
    /// to tell the chats that a reload of im.toml newly serves.
    pub(crate) fn serves(&self, platform: &str, chat: &ConversationId) -> bool {
        lock(&self.0)
            .conversations
            .get(platform)
            .is_some_and(|chats| chats.contains(chat))
    }

    /// Whether a click by `user` on `platform` decides approvals in a chat without approvers of
    /// its own.
    pub(super) fn approves(&self, platform: &str, user: &UserId) -> bool {
        lock(&self.0)
            .approvers
            .get(platform)
            .is_some_and(|users| users.contains(user))
    }
}
