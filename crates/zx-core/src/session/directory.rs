//! Session files kept in one directory: list them and find one to resume
//! (pi: `SessionManager.list`, `findMostRecentSession`, `findById`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::{Entry, SessionStore};
use crate::ai::{Message, UserMessageKind};

/// One session file in a directory, as [`list_sessions`] read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    pub path: PathBuf,
    /// The session ID from the header.
    pub id: String,
    /// The working directory the session was started in.
    pub cwd: String,
    /// When the session was created, in milliseconds since the Unix epoch.
    pub started_ms: u64,
    /// When the last record was written; the start for a session without records.
    pub last_activity_ms: u64,
    /// User and assistant messages in the model's context.
    pub messages: usize,
    /// The first prompt typed into the session, as typed.
    pub first_prompt: Option<String>,
}

/// A file that looked like a session but could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unreadable {
    pub path: PathBuf,
    pub error: String,
}

/// The sessions of one directory, most recently active first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionList {
    pub sessions: Vec<SessionInfo>,
    /// `.jsonl` files that are not sessions, or are damaged; they are left as they are.
    pub unreadable: Vec<Unreadable>,
}

impl SessionList {
    /// The most recently active session started in `cwd`, compared as the header records it
    /// (the canonical path for sessions the CLI creates).
    pub fn latest_in(&self, cwd: &Path) -> Option<&SessionInfo> {
        let cwd = cwd.display().to_string();
        self.sessions.iter().find(|session| session.cwd == cwd)
    }

    /// The one session whose ID starts with `prefix`. Fails when none or several match, or
    /// when `prefix` is empty.
    pub fn find(&self, prefix: &str) -> Result<&SessionInfo> {
        if prefix.is_empty() {
            bail!("an empty session ID matches every session");
        }
        let mut matches = self.sessions.iter().filter(|session| session.id.starts_with(prefix));
        match (matches.next(), matches.next()) {
            (Some(session), None) => Ok(session),
            (None, _) => bail!("no session ID starts with {prefix}"),
            (Some(_), Some(_)) => bail!("more than one session ID starts with {prefix}; give more of the ID"),
        }
    }
}

/// Read every `*.jsonl` file directly in `dir`, without taking its lock or changing it, as
/// [`SessionStore::read`] does: a session another process is writing is listed as of now.
/// A missing directory has no sessions. Each file is read whole, so listing takes time in
/// proportion to the sessions' size; this is blocking file I/O.
pub fn list_sessions(dir: &Path) -> Result<SessionList> {
    let read_dir = match std::fs::read_dir(dir) {
        Ok(read_dir) => read_dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(SessionList::default()),
        Err(error) => return Err(error).with_context(|| format!("cannot list sessions in {}", dir.display())),
    };
    let mut list = SessionList::default();
    for item in read_dir {
        let item = item.with_context(|| format!("cannot list sessions in {}", dir.display()))?;
        let path = item.path();
        // A directory named like a session is not one; neither is a link that leads nowhere.
        if path.extension().is_none_or(|extension| extension != "jsonl") || !path.is_file() {
            continue;
        }
        match SessionStore::read(&path) {
            Ok(store) => list.sessions.push(info(path, &store)),
            Err(error) => list.unreadable.push(Unreadable {
                path,
                error: format!("{error:#}"),
            }),
        }
    }
    // Newest first; the ID breaks ties, so the order does not depend on the directory's.
    list.sessions.sort_by(|a, b| {
        b.last_activity_ms
            .cmp(&a.last_activity_ms)
            .then_with(|| a.id.cmp(&b.id))
    });
    list.unreadable.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(list)
}

fn info(path: PathBuf, store: &SessionStore) -> SessionInfo {
    let header = store.header();
    let first_prompt = store.entries().iter().find_map(|entry| match entry {
        Entry::Message(entry) => match &entry.message {
            Message::User {
                content,
                kind: UserMessageKind::Prompt,
                ..
            } => Some(content.clone()),
            Message::User { .. } | Message::System(_) | Message::Assistant(_) | Message::ToolResult(_) => None,
        },
        Entry::ContextEdit(_)
        | Entry::FormatUpgrade(_)
        | Entry::CompactionStarted(_)
        | Entry::Compaction(_)
        | Entry::CompactionFailed(_)
        | Entry::RequestConfig(_)
        | Entry::RequestStarted(_)
        | Entry::ToolCallStarted(_)
        | Entry::Custom(_)
        | Entry::CustomMessage(_) => None,
    });
    SessionInfo {
        id: header.id.clone(),
        cwd: header.cwd.clone(),
        started_ms: header.timestamp,
        last_activity_ms: store
            .entries()
            .last()
            .map_or(header.timestamp, |entry| entry.timestamp()),
        messages: store
            .messages()
            .iter()
            .filter(|message| matches!(message, Message::User { .. } | Message::Assistant(_)))
            .count(),
        first_prompt,
        path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, cwd: &str, last_activity_ms: u64) -> SessionInfo {
        SessionInfo {
            path: PathBuf::from(format!("{id}.jsonl")),
            id: id.into(),
            cwd: cwd.into(),
            started_ms: 0,
            last_activity_ms,
            messages: 0,
            first_prompt: None,
        }
    }

    #[test]
    fn find_needs_a_prefix_that_matches_exactly_one_session() {
        let list = SessionList {
            sessions: vec![session("abc1", "/w", 2), session("abd2", "/w", 1)],
            unreadable: vec![],
        };
        assert_eq!(list.find("abc").unwrap().id, "abc1");
        assert!(list.find("ab").unwrap_err().to_string().contains("more than one"));
        assert!(
            list.find("x")
                .unwrap_err()
                .to_string()
                .contains("no session ID starts with x")
        );
        assert!(list.find("").is_err());
    }

    #[test]
    fn latest_in_skips_sessions_of_other_directories() {
        let list = SessionList {
            sessions: vec![session("new", "/other", 3), session("old", "/w", 1)],
            unreadable: vec![],
        };
        assert_eq!(list.latest_in(Path::new("/w")).unwrap().id, "old");
        assert!(list.latest_in(Path::new("/none")).is_none());
    }
}
