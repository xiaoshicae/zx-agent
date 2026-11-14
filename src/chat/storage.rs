//! Where a chat keeps its conversations, and which session it opens first.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use zx_agent::transcript::SessionQuery;

use crate::cli;
use crate::render::visible;
use crate::session_files::SessionPlace;

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
    let (place, original) = match (no_session, session) {
        (true, _) => (SessionPlace::memory(), None),
        (false, Some(path)) => (SessionPlace::file(&path), Some(path)),
        (false, None) => (SessionPlace::home()?, None),
    };
    let first = match (&place, resume) {
        (SessionPlace::Directory(_), Some(id)) => resume_target(&place, cwd, id).await?,
        // `--session` continues the session its file holds, or starts the file.
        (SessionPlace::File(_), _) => match place.only().await? {
            Some(held) => {
                cli::check_session_cwd(cwd, &held.cwd)?;
                First::Resume(held.id)
            }
            None => First::Fresh,
        },
        (SessionPlace::Memory(_) | SessionPlace::Directory(_), _) => First::Fresh,
    };
    Ok((Storage { place, original }, first))
}

/// The session `--resume` opens among those started in this working directory, or why there
/// is none to resume.
async fn resume_target(place: &SessionPlace, cwd: Option<&Path>, id: Option<&str>) -> Result<First> {
    let cwd = cli::resolve_cwd(cwd)?;
    let list = place
        .list(&SessionQuery {
            cwd: Some(cwd.clone()),
            ..SessionQuery::default()
        })
        .await?;
    let directory = place.location(&cwd).unwrap_or_default();
    match id {
        Some(id) => {
            let session = list
                .find(id)
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

/// For /new with `--session`: a new file beside the original.
fn new_file(original: &Path) -> PathBuf {
    let mut name = original
        .file_stem()
        .unwrap_or_else(|| OsStr::new("session"))
        .to_os_string();
    name.push(format!(".{}.jsonl", uuid::Uuid::new_v4()));
    original.with_file_name(name)
}
