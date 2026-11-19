//! Where a chat keeps its conversations, and which session it opens first.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use zx_agent::transcript::{SessionInfo, SessionList};
use zx_app::{SessionPlace, resolve_cwd};

use crate::render::visible;
use crate::session_file;

/// Where conversations are kept.
pub(super) struct Storage {
    /// Where the first conversation is kept: memory with `--no-session`, the file `--session`
    /// names, else the session files under the home directory.
    pub(super) place: SessionPlace,
    /// `--session`'s file: /new creates a file beside it for the next conversation.
    original: Option<PathBuf>,
}

impl Storage {
    /// Where /new keeps the next conversation when that is not where the first one is: a new
    /// file beside `--session`'s.
    pub(super) fn beside(&self) -> Option<SessionPlace> {
        self.original
            .as_deref()
            .map(|original| SessionPlace::file(&new_file(original)))
    }
}

/// The session a conversation opens with.
pub(super) enum First {
    /// A session to continue, by its ID: the one `--resume` named, the latest one of this
    /// directory, or the one `--session`'s file holds already.
    Resume(String),
    /// A new session.
    Fresh,
    /// A new session, because `--resume` found none of this directory; the line says so.
    NothingToResume(String),
}

impl First {
    /// The session to open, when an earlier one is continued.
    pub(super) fn id(&self) -> Option<&str> {
        match self {
            Self::Resume(id) => Some(id),
            Self::Fresh | Self::NothingToResume(_) => None,
        }
    }

    /// A line to show about the choice, once the conversation is built.
    pub(super) fn notice(&self) -> Option<String> {
        match self {
            Self::NothingToResume(line) => Some(line.clone()),
            Self::Resume(_) | Self::Fresh => None,
        }
    }
}

/// Where the command line keeps a chat's conversations: `--no-session`, `--session` and
/// `--resume` exclude each other.
pub(super) enum Keeping<'a> {
    /// `--no-session`: in memory.
    Memory,
    /// `--session`: in this file, continuing the session it holds.
    File(PathBuf),
    /// In the session files of the home directory, continuing the session `--resume` names,
    /// when it is given.
    Home(Option<Resume<'a>>),
}

impl<'a> Keeping<'a> {
    /// Where `--no-session`, `--session` and `--resume` keep the conversations. The parser
    /// refuses `--no-session` with either of the others, and `--resume` with `--session`.
    pub(super) fn of(no_session: bool, session: Option<PathBuf>, resume: Option<&'a Option<String>>) -> Self {
        match (no_session, session) {
            (true, _) => Self::Memory,
            (false, Some(path)) => Self::File(path),
            (false, None) => Self::Home(resume.map(|id| id.as_deref().map_or(Resume::Latest, Resume::Prefix))),
        }
    }
}

/// The session `--resume` continues, of those started in the working directory.
pub(super) enum Resume<'a> {
    /// The most recent one.
    Latest,
    /// The one whose ID starts with this.
    Prefix(&'a str),
}

/// Where the conversations are kept, and the session to open first: as `keeping` says, in
/// `cwd`, the command line's working directory.
pub(super) async fn opening(keeping: Keeping<'_>, cwd: Option<&Path>) -> Result<(Storage, First)> {
    let (place, original, first) = match keeping {
        Keeping::Memory => (SessionPlace::memory(), None, First::Fresh),
        // `--session` continues the session its file holds, or starts the file.
        Keeping::File(path) => {
            let (place, held) = session_file::open(&path, cwd).await?;
            (place, Some(path), held.map_or(First::Fresh, First::Resume))
        }
        Keeping::Home(resume) => {
            let place = SessionPlace::home()?;
            let first = match resume {
                Some(resume) => resume_target(&place, cwd, resume).await?,
                None => First::Fresh,
            };
            (place, None, first)
        }
    };
    Ok((Storage { place, original }, first))
}

/// The session `--resume` opens among those started in this working directory, or why there
/// is none to resume.
async fn resume_target(place: &SessionPlace, cwd: Option<&Path>, resume: Resume<'_>) -> Result<First> {
    let cwd = resolve_cwd(cwd)?;
    let list = place.started_in(&cwd).await?;
    let directory = place.location(&cwd).unwrap_or_default();
    match resume {
        Resume::Prefix(id) => {
            let session = session_with_id_prefix(&list, id)
                .with_context(|| format!("--resume: no single session to resume in {}", directory.display()))?;
            Ok(First::Resume(session.id.clone()))
        }
        Resume::Latest => Ok(match list.sessions.first() {
            Some(session) => First::Resume(session.id.clone()),
            None => First::NothingToResume(format!(
                "[chat] no earlier session of this directory in {}; starting a new one",
                visible(&directory.display().to_string())
            )),
        }),
    }
}

/// The one session of `list` whose ID starts with `prefix`. Fails when none or several match,
/// or when `prefix` is empty.
fn session_with_id_prefix<'a>(list: &'a SessionList, prefix: &str) -> Result<&'a SessionInfo> {
    if prefix.is_empty() {
        bail!("an empty session ID matches every session");
    }
    let mut matches = list.sessions.iter().filter(|session| session.id.starts_with(prefix));
    match (matches.next(), matches.next()) {
        (Some(session), None) => Ok(session),
        (None, _) => bail!("no session ID starts with {prefix}"),
        (Some(_), Some(_)) => bail!("more than one session ID starts with {prefix}; give more of the ID"),
    }
}

/// For /new with `--session`: a new file beside the original.
fn new_file(original: &Path) -> PathBuf {
    let mut name = original
        .file_stem()
        .unwrap_or_else(|| OsStr::new("session"))
        .to_os_string();
    name.push(format!(".{}.jsonl", uuid::Uuid::new_v4()));
    original.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;

    fn session(id: &str, last_activity_ms: u64) -> SessionInfo {
        SessionInfo {
            cwd: "/w".into(),
            last_activity_ms,
            ..test_support::session(id)
        }
    }

    #[tokio::test]
    async fn a_conversation_kept_in_memory_continues_nothing() {
        let (storage, first) = opening(Keeping::Memory, None).await.unwrap();
        assert!(!storage.place.is_persistent());
        assert_eq!(first.id(), None);
        assert!(storage.beside().is_none());
    }

    #[test]
    fn resuming_by_id_needs_a_prefix_that_matches_exactly_one_session() {
        let list = SessionList {
            sessions: vec![session("abc1", 2), session("abd2", 1)],
            unreadable: vec![],
        };
        assert_eq!(session_with_id_prefix(&list, "abc").unwrap().id, "abc1");
        assert!(
            session_with_id_prefix(&list, "ab")
                .unwrap_err()
                .to_string()
                .contains("more than one")
        );
        assert!(
            session_with_id_prefix(&list, "x")
                .unwrap_err()
                .to_string()
                .contains("no session ID starts with x")
        );
        assert!(session_with_id_prefix(&list, "").is_err());
    }
}
