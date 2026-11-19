//! Where sessions are kept: the storage interface, what a listing is asked for and what it
//! returns (pi: `SessionManager.list`, `findMostRecentSession`).

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use super::{Entry, LoadedSession, SessionHeader, SessionLog, SessionName, Transcript};
use crate::ai::{Message, UserMessageKind};

/// Where sessions are kept: memory ([`MemoryStorage`](super::MemoryStorage)), session files
/// (`JsonlStorage` of the zx-builtin crate), or a backend of the host's, such as a database.
///
/// A storage keeps each session's header and, through the [`SessionLog`] it hands out, the
/// session's records in order. It does not interpret the records: [`Transcript`] checks
/// them when a session is loaded and before each append. An implementation guarantees:
///
/// - [`SessionLog::append`] returns only once the record is stored.
/// - A record whose `seq` the session already has is refused, never overwritten.
/// - A session has one writer at a time: while a log of it exists, `open` fails, or the
///   appends of a second writer do, as a unique key on the session ID and `seq` makes them.
///
/// # Example
///
/// A session created in a storage, written, and continued later from the same storage:
///
/// ```
/// use std::path::Path;
///
/// use zx_ai::Message;
/// use zx_core::transcript::{MemoryStorage, SessionHeader, SessionQuery, SessionStorage, Transcript};
///
/// # #[tokio::main]
/// # async fn main() -> anyhow::Result<()> {
/// let storage = MemoryStorage::default();
/// let header = SessionHeader::new(Path::new("."), "model");
/// let log = storage.create(&header).await?;
/// let mut session = Transcript::with_log(header, log);
/// session.append_message(Message::user("check the disk")).await?;
/// let id = session.header().id.clone();
/// // One writer at a time: the session is released before it is opened again.
/// drop(session);
///
/// let (loaded, log) = storage.open(&id).await?;
/// let continued = Transcript::load(loaded, log)?;
/// assert_eq!(continued.messages().len(), 1);
/// assert_eq!(storage.list(&SessionQuery::default()).await?.sessions[0].id, id);
/// # Ok(())
/// # }
/// ```
#[async_trait]
pub trait SessionStorage: Send + Sync {
    /// Store `header` as a new session and return the log its records go to. Fails when a
    /// session with that ID exists already.
    async fn create(&self, header: &SessionHeader) -> Result<Box<dyn SessionLog>>;

    /// Read session `id` back and become its writer: the header and records, for
    /// [`Transcript::load`], and the log that continues the session. Fails when there is no
    /// such session.
    async fn open(&self, id: &str) -> Result<(LoadedSession, Box<dyn SessionLog>)>;

    /// Read session `id` without becoming its writer, also while another writer has it
    /// open: a snapshot, for [`Transcript::snapshot`]. Fails when there is no such session.
    async fn read(&self, id: &str) -> Result<LoadedSession>;

    /// The headers of the sessions `query` asks for, the most recently started first and no
    /// more than its limit, as [`SessionQuery::arrange_headers`] arranges them: what a host
    /// that keeps an index of its sessions reads, without their records. A session whose
    /// header cannot be read is left out; [`list`](Self::list) reports it. The default reads
    /// every session `list` names; a storage that can read headers alone does so instead.
    async fn headers(&self, query: &SessionQuery) -> Result<Vec<SessionHeader>> {
        let listed = self
            .list(&SessionQuery {
                limit: None,
                ..query.clone()
            })
            .await?;
        let mut headers = Vec::with_capacity(listed.sessions.len());
        for session in listed.sessions {
            // A session gone since the listing has no header to give.
            if let Ok(loaded) = self.read(&session.id).await {
                headers.push(loaded.header);
            }
        }
        Ok(query.arrange_headers(headers))
    }

    /// The sessions `query` asks for, most recently active first and no more than its limit,
    /// as [`SessionList::new`] arranges them. A session being written is listed as of now.
    async fn list(&self, query: &SessionQuery) -> Result<SessionList>;
}

/// Which sessions a listing returns. The default asks for every session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionQuery {
    /// Only sessions started in this working directory, compared as the header records it:
    /// the absolute path that [`Agent::create_session`](crate::Agent::create_session) made of
    /// the directory it was given.
    pub cwd: Option<PathBuf>,
    /// Only sessions that carry each of these labels with the same value.
    pub labels: BTreeMap<String, String>,
    /// At most this many sessions: the most recently active ones.
    pub limit: Option<usize>,
}

impl SessionQuery {
    /// Whether a session with `header` is one the query asks for, whatever the limit.
    pub fn matches(&self, header: &SessionHeader) -> bool {
        self.cwd
            .as_ref()
            .is_none_or(|cwd| header.cwd == cwd.display().to_string())
            && self
                .labels
                .iter()
                .all(|(name, value)| header.labels.get(name) == Some(value))
    }

    /// The `headers` that match, the most recently started first, then by ID, and no more
    /// than the limit: what [`SessionStorage::headers`] returns.
    pub fn arrange_headers(&self, mut headers: Vec<SessionHeader>) -> Vec<SessionHeader> {
        headers.retain(|header| self.matches(header));
        headers.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then_with(|| a.id.cmp(&b.id)));
        if let Some(limit) = self.limit {
            headers.truncate(limit);
        }
        headers
    }

    /// `sessions`, each of which matches, in the order and number [`SessionList::new`] gives.
    fn arrange(&self, mut sessions: Vec<SessionInfo>) -> Vec<SessionInfo> {
        // Newest first: a session started in the millisecond another was last written to is
        // the newer one. The ID breaks the ties left, so the order does not depend on the
        // storage's.
        sessions.sort_by(|a, b| {
            b.last_activity_ms
                .cmp(&a.last_activity_ms)
                .then_with(|| b.started_ms.cmp(&a.started_ms))
                .then_with(|| a.id.cmp(&b.id))
        });
        if let Some(limit) = self.limit {
            sessions.truncate(limit);
        }
        sessions
    }
}

/// One session in a listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
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
    /// The session's name, if it was given one.
    pub name: Option<SessionName>,
    /// The labels the session was created with.
    pub labels: BTreeMap<String, String>,
}

impl SessionInfo {
    /// What a listing shows of the session in `transcript`. A storage that keeps no listing
    /// of its sessions builds it from a snapshot ([`Transcript::snapshot`]).
    pub fn of(transcript: &Transcript) -> Self {
        let header = transcript.header();
        let first_prompt = transcript.entries().iter().find_map(|entry| {
            let message = entry.projected()?;
            match &*message {
                Message::User {
                    content,
                    kind: UserMessageKind::Prompt,
                    ..
                } => Some(content.clone()),
                Message::User { .. } | Message::System(_) | Message::Assistant(_) | Message::ToolResult(_) => None,
            }
        });
        Self {
            id: header.id.clone(),
            cwd: header.cwd.clone(),
            started_ms: header.timestamp,
            last_activity_ms: transcript.entries().last().map_or(header.timestamp, Entry::timestamp),
            messages: transcript
                .messages()
                .iter()
                .filter(|message| matches!(message, Message::User { .. } | Message::Assistant(_)))
                .count(),
            first_prompt,
            name: transcript.session_name().cloned(),
            labels: header.labels.clone(),
        }
    }

    /// What a listing shows for the session: its name, else its first prompt.
    pub fn title(&self) -> Option<&str> {
        self.name
            .as_ref()
            .map(SessionName::as_str)
            .or(self.first_prompt.as_deref())
    }
}

/// A session the storage holds but could not read, such as a damaged file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unreadable {
    /// Where the storage keeps it, in the storage's own terms: a file path, for session
    /// files.
    pub location: String,
    /// Why it could not be read, naming the location: the whole message to report.
    pub error: String,
}

/// What a listing returns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionList {
    /// The sessions asked for, most recently active first.
    pub sessions: Vec<SessionInfo>,
    /// What the storage found where it looked for those sessions and could not read; it is
    /// left as it is.
    pub unreadable: Vec<Unreadable>,
}

impl SessionList {
    /// The listing `query` asks for, of `sessions` that each match it: most recently active
    /// first, of those last active at the same moment the one started later first, and no
    /// more of them than the limit; `unreadable` in the order of their locations, so that a
    /// listing does not depend on the order in which the storage found what it holds.
    pub fn new(query: &SessionQuery, sessions: Vec<SessionInfo>, mut unreadable: Vec<Unreadable>) -> Self {
        unreadable.sort_by(|a, b| a.location.cmp(&b.location));
        Self {
            sessions: query.arrange(sessions),
            unreadable,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn session(id: &str, last_activity_ms: u64) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            cwd: "/w".into(),
            started_ms: 0,
            last_activity_ms,
            messages: 0,
            first_prompt: None,
            name: None,
            labels: BTreeMap::new(),
        }
    }

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn a_title_is_the_name_else_the_first_prompt() {
        let mut listed = session("abc1", 1);
        assert_eq!(listed.title(), None);
        listed.first_prompt = Some("fix the build".into());
        assert_eq!(listed.title(), Some("fix the build"));
        listed.name = SessionName::new("build fix");
        assert_eq!(listed.title(), Some("build fix"));
    }

    #[test]
    fn a_query_matches_the_working_directory_and_every_label_it_names() {
        let header = SessionHeader {
            labels: labels(&[("tenant", "acme"), ("channel", "web")]),
            ..SessionHeader::new(Path::new("/w"), "m")
        };
        let matches = |query: SessionQuery| query.matches(&header);

        assert!(matches(SessionQuery::default()));
        assert!(matches(SessionQuery {
            cwd: Some("/w".into()),
            labels: labels(&[("tenant", "acme")]),
            limit: Some(0),
        }));
        assert!(!matches(SessionQuery {
            cwd: Some("/elsewhere".into()),
            ..SessionQuery::default()
        }));
        for other in [
            labels(&[("tenant", "other")]),
            labels(&[("tenant", "acme"), ("team", "core")]),
        ] {
            assert!(!matches(SessionQuery {
                labels: other,
                ..SessionQuery::default()
            }));
        }
    }

    #[test]
    fn a_listing_is_most_recently_active_first_and_no_longer_than_the_limit() {
        let sessions = || vec![session("b", 1), session("c", 3), session("a", 1), session("d", 2)];
        let ids = |query: SessionQuery| -> Vec<String> {
            query
                .arrange(sessions())
                .into_iter()
                .map(|session| session.id)
                .collect()
        };

        assert_eq!(ids(SessionQuery::default()), ["c", "d", "a", "b"]);
        let limited = SessionQuery {
            limit: Some(2),
            ..SessionQuery::default()
        };
        assert_eq!(ids(limited), ["c", "d"]);
    }

    #[test]
    fn of_sessions_last_active_at_the_same_moment_the_one_started_later_comes_first() {
        // A session started in the millisecond another was last written to is the newer one.
        let older = SessionInfo {
            started_ms: 1,
            ..session("a", 5)
        };
        let newer = SessionInfo {
            started_ms: 5,
            ..session("z", 5)
        };

        let arranged = SessionQuery::default().arrange(vec![older, newer]);

        let ids: Vec<&str> = arranged.iter().map(|session| session.id.as_str()).collect();
        assert_eq!(ids, ["z", "a"]);
    }
}
