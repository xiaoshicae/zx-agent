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

/// Where the conversations are kept, and the session to open first: `--no-session`,
/// `--session`, `--resume` with its ID if it has one, and `cwd`, the command line's working
/// directory.
pub(super) async fn opening(
    no_session: bool,
    session: Option<PathBuf>,
    resume: Option<Option<&str>>,
    cwd: Option<&Path>,
) -> Result<(Storage, First)> {
    // The parser refuses `--no-session` with `--session` or `--resume`, and `--resume` with
    // `--session`.
    let (place, original, first) = match (no_session, session) {
        (true, _) => (SessionPlace::memory(), None, First::Fresh),
        // `--session` continues the session its file holds, or starts the file.
        (false, Some(path)) => {
            let (place, held) = session_file::open(&path, cwd).await?;
            (place, Some(path), held.map_or(First::Fresh, First::Resume))
        }
        (false, None) => {
            let place = SessionPlace::home()?;
            let first = match resume {
                Some(id) => resume_target(&place, cwd, id).await?,
                None => First::Fresh,
            };
            (place, None, first)
        }
    };
    Ok((Storage { place, original }, first))
}

/// The session `--resume` opens among those started in this working directory, or why there
/// is none to resume.
async fn resume_target(place: &SessionPlace, cwd: Option<&Path>, id: Option<&str>) -> Result<First> {
    let cwd = resolve_cwd(cwd)?;
    let list = place.started_in(&cwd).await?;
    let directory = place.location(&cwd).unwrap_or_default();
    match id {
        Some(id) => {
            let session = session_with_id_prefix(&list, id)
                .with_context(|| format!("--resume: no single session to resume in {}", directory.display()))?;
            Ok(First::Resume(session.id.clone()))
        }
        None => Ok(match list.sessions.first() {
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
    use std::collections::BTreeMap;

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

    #[tokio::test]
    async fn a_conversation_kept_in_memory_continues_nothing() {
        let (storage, first) = opening(true, None, None, None).await.unwrap();
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
