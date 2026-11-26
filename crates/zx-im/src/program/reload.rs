//! The lists of im.toml, read again while the program runs: on SIGHUP, its `[access]`
//! replaces the server's lists, once its server's sections pass the checks of a start. A
//! replica that waits for its turn to serve reloads too, and serves with what it read. What
//! chats have of their own stays as the start read it: a chat the new lists no longer hold
//! only leaves its settings unused, in the replica that leads and in the one that takes over.
//! A reload does not start to serve a chat with other settings than the file gives it: a chat
//! that the file newly serves, with a `[[chats]]` entry other than the one in effect, stays off
//! the lists until a restart. A chat served already stays served with the settings in effect
//! when the file changes them. The log names both ([`plan`]).

use std::collections::HashSet;
use std::path::PathBuf;

use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::config_file::{OwnChat, ServerSections, read_file};
use super::log;
use crate::{Access, ConversationId, UserId};

/// What the start settled and every reload goes by: where im.toml is, the `platforms` the
/// server runs, and what `[[chats]]` gave chats of their own, in effect until a restart.
pub(super) struct Started {
    pub(super) path: PathBuf,
    pub(super) platforms: Vec<String>,
    pub(super) chats: Vec<OwnChat>,
}

/// Read the im.toml of `started` again on each SIGHUP until `cancel`, and give `access` its
/// lists, but for the chats that wait for a restart ([`plan`]). The signal is listened for
/// before this returns, so that a SIGHUP from then on reloads rather than ends the process. A
/// file that cannot be read, or whose server's sections fail the checks, leaves the lists as
/// they are, and the log says why. Without a way to listen for the signal, that is logged,
/// nothing reloads and there is no task.
pub(super) fn on_hangup(started: Started, access: Access, cancel: CancellationToken) -> Option<JoinHandle<()>> {
    match signal(SignalKind::hangup()) {
        Ok(hangups) => Some(tokio::spawn(reload_on(hangups, started, access, cancel))),
        Err(error) => {
            log::failed(&format!(
                "cannot listen for SIGHUP; {} is read again only by a restart: {error}",
                started.path.display()
            ));
            None
        }
    }
}

/// Reload the file of `started` on each of `hangups`, until `cancel`.
async fn reload_on(mut hangups: Signal, started: Started, access: Access, cancel: CancellationToken) {
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            received = hangups.recv() => {
                if received.is_none() {
                    return;
                }
            }
        }
        // A small, bounded read of the file, as rare as the signal.
        reload(&started, &access);
    }
}

/// Read the im.toml of `started` again and give `access` the lists it plans, or log why not;
/// the log names each chat whose own settings wait for a restart.
fn reload(started: &Started, access: &Access) {
    let Started { path, platforms, chats } = started;
    let names: HashSet<&str> = platforms.iter().map(String::as_str).collect();
    match read_file(path).and_then(|file| file.server_sections(&names)) {
        Ok(sections) => {
            let Plan {
                conversations,
                approvers,
                held_back,
                changed,
            } = plan(|platform, chat| access.serves(platform, chat), chats, sections);
            log::reloaded(conversations.len(), approvers.len());
            access.replace(conversations, approvers);
            for (platform, chat) in &held_back {
                log::held_back(platform, chat);
            }
            for (platform, chat) in &changed {
                log::settings_wait(platform, chat);
            }
        }
        Err(error) => log::failed(&format!(
            "cannot reload {}; the lists stay as they were: {error:#}",
            path.display()
        )),
    }
}

/// What a reload does with the file it read: the lists it gives the server, and the chats
/// whose own settings wait for a restart, which it tells the operator.
struct Plan {
    conversations: Vec<(String, ConversationId)>,
    approvers: Vec<(String, UserId)>,
    /// Chats the file serves that were not served, with other settings of their own than
    /// those in effect: left off `conversations`, and served after a restart.
    held_back: Vec<(String, ConversationId)>,
    /// Chats served before and after, whose own settings the file changes: served with those
    /// in effect until a restart.
    changed: Vec<(String, ConversationId)>,
}

/// What a reload of `file` does in a server that answers where `was_served` says, with the
/// chats' own settings `in_effect`, those of the start.
///
/// The server serves a chat with the settings in effect, whatever the file gives it. So a
/// chat that the file serves with other settings of its own is left off the lists when it was
/// not served, to be served after a restart, and stays served when it was, with the settings
/// in effect; the plan names both. The settings are the same when the file gives the chat the
/// entry of `[[chats]]` that the start read, or none as the start read none ([`OwnChat`]).
fn plan(was_served: impl Fn(&str, &ConversationId) -> bool, in_effect: &[OwnChat], file: ServerSections) -> Plan {
    let ServerSections {
        conversations: listed,
        approvers,
        chats,
        // A restart's, like the chats' own settings.
        daily: _,
        schedules: _,
        sessions: _,
    } = file;
    let mut plan = Plan {
        conversations: Vec::with_capacity(listed.len()),
        approvers,
        held_back: Vec::new(),
        changed: Vec::new(),
    };
    for (platform, chat) in listed {
        if own(&chats, &platform, &chat) == own(in_effect, &platform, &chat) {
            plan.conversations.push((platform, chat));
        } else if was_served(&platform, &chat) {
            // Named and served: one copy for each list.
            plan.changed.push((platform.clone(), chat.clone()));
            plan.conversations.push((platform, chat));
        } else {
            plan.held_back.push((platform, chat));
        }
    }
    plan
}

/// What `chats` give the chat `chat` on `platform` of its own, if anything.
fn own<'a>(chats: &'a [OwnChat], platform: &str, chat: &ConversationId) -> Option<&'a OwnChat> {
    chats.iter().find(|own| own.platform == platform && own.chat == *chat)
}

#[cfg(test)]
mod tests {
    use super::super::config_file::ConfigFile;
    use super::*;

    /// The agent and the approver that slack:C2 has of its own.
    const PROD: &str = "[[chats]]\nchat = \"slack:C2\"\nagent = \"prod\"\napprovers = [\"slack:U9\"]\n";

    /// The server's sections of an im.toml that serves `chats` on slack, where U1 approves,
    /// with `own` after them.
    fn file(chats: &[&str], own: &str) -> ServerSections {
        let listed: Vec<String> = chats.iter().map(|chat| format!("\"slack:{chat}\"")).collect();
        let text = format!(
            "[access]\nconversations = [{}]\napprovers = [\"slack:U1\"]\n[sessions]\ndir = \"d\"\n{own}",
            listed.join(", ")
        );
        let file = ConfigFile::parse(&text).unwrap();
        file.server_sections(&HashSet::from(["slack"])).unwrap()
    }

    /// What a reload of `file` does in a server that serves `served` on slack and was started
    /// with `start`.
    fn planned(served: &[&str], start: &ServerSections, file: ServerSections) -> Plan {
        let was_served =
            |platform: &str, chat: &ConversationId| platform == "slack" && served.contains(&chat.0.as_str());
        plan(was_served, &start.chats, file)
    }

    fn slack(chat: &str) -> (String, ConversationId) {
        ("slack".into(), ConversationId(chat.into()))
    }

    #[test]
    fn a_new_chat_with_settings_of_its_own_is_held_back_and_named() {
        let start = file(&["C1"], "");
        let plan = planned(&["C1"], &start, file(&["C1", "C2"], PROD));
        assert_eq!(plan.conversations, [slack("C1")]);
        assert_eq!(plan.held_back, [slack("C2")]);
        assert_eq!(plan.changed, []);
    }

    #[test]
    fn a_new_chat_without_settings_of_its_own_is_served() {
        let start = file(&["C1"], "");
        let plan = planned(&["C1"], &start, file(&["C1", "C2"], ""));
        assert_eq!(plan.conversations, [slack("C1"), slack("C2")]);
        assert_eq!(plan.held_back, []);
        assert_eq!(plan.changed, []);
    }

    #[test]
    fn a_new_chat_with_the_settings_in_effect_is_served() {
        // Taken off the lists by an earlier reload, with its entry, and now back with it.
        let start = file(&["C1", "C2"], PROD);
        let plan = planned(&["C1"], &start, file(&["C1", "C2"], PROD));
        assert_eq!(plan.conversations, [slack("C1"), slack("C2")]);
        assert_eq!(plan.held_back, []);
        assert_eq!(plan.changed, []);
    }

    #[test]
    fn a_new_chat_without_the_settings_in_effect_for_it_is_held_back_and_named() {
        // Back on the lists without the entry the start read: it would be served with that one.
        let start = file(&["C1", "C2"], PROD);
        let plan = planned(&["C1"], &start, file(&["C1", "C2"], ""));
        assert_eq!(plan.conversations, [slack("C1")]);
        assert_eq!(plan.held_back, [slack("C2")]);
        assert_eq!(plan.changed, []);
    }

    #[test]
    fn a_served_chat_whose_settings_the_file_changes_or_drops_stays_served_and_is_named() {
        let start = file(&["C1", "C2"], PROD);
        for own in [PROD.replace("slack:U9", "slack:U8"), String::new()] {
            let plan = planned(&["C1", "C2"], &start, file(&["C1", "C2"], &own));
            assert_eq!(plan.conversations, [slack("C1"), slack("C2")], "{own}");
            assert_eq!(plan.held_back, [], "{own}");
            assert_eq!(plan.changed, [slack("C2")], "{own}");
        }
    }

    #[test]
    fn a_served_chat_that_the_file_gives_settings_of_its_own_stays_served_and_is_named() {
        let start = file(&["C1", "C2"], "");
        let plan = planned(&["C1", "C2"], &start, file(&["C1", "C2"], PROD));
        assert_eq!(plan.conversations, [slack("C1"), slack("C2")]);
        assert_eq!(plan.held_back, []);
        assert_eq!(plan.changed, [slack("C2")]);
    }

    #[test]
    fn a_chat_the_file_no_longer_serves_is_dropped() {
        let start = file(&["C1", "C2"], PROD);
        let plan = planned(&["C1", "C2"], &start, file(&["C1"], ""));
        assert_eq!(plan.conversations, [slack("C1")]);
        assert_eq!(plan.held_back, []);
        assert_eq!(plan.changed, []);
    }

    #[test]
    fn the_approvers_are_those_of_the_file_whatever_is_held_back() {
        let start = file(&["C1"], "");
        let plan = planned(&["C1"], &start, file(&["C1", "C2"], PROD));
        assert_eq!(plan.approvers, [("slack".to_string(), UserId("U1".into()))]);
    }
}
