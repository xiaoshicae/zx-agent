//! Where a chat keeps its conversations, and which session it opens first.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::cli::{self, BaseArgs};
use crate::render::visible;
use crate::session_files;

/// Where conversations are kept.
pub(super) enum Storage {
    /// `--no-session`.
    Memory,
    /// `--session`: the file named; /new creates one beside it.
    File(PathBuf),
    /// A session directory: every new conversation gets its own file there.
    Directory(PathBuf),
}

/// The session a conversation opens with: what `--resume` led to.
pub(super) enum First {
    /// The session `--resume` named, or the latest one of this directory.
    Resume(PathBuf),
    /// A new session.
    Fresh,
    /// A new session, because `--resume` found none of this directory; the line says so.
    NothingToResume(String),
}

impl First {
    /// The file to open, when an earlier session is continued.
    pub(super) fn path(&self) -> Option<&PathBuf> {
        match self {
            Self::Resume(path) => Some(path),
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
/// `--resume` with its ID if it has one, and the command line's session file and working
/// directory.
pub(super) async fn opening(
    no_session: bool,
    resume: Option<Option<&str>>,
    base: &BaseArgs,
) -> Result<(Storage, First)> {
    let storage = if no_session {
        Storage::Memory
    } else if let Some(path) = &base.session {
        Storage::File(path.clone())
    } else {
        Storage::Directory(session_files::default_dir(&cli::resolve_cwd(base.cwd.as_deref())?)?)
    };
    let first = match (&storage, resume) {
        (Storage::Directory(dir), Some(id)) => resume_target(dir, base.cwd.as_deref(), id).await?,
        (Storage::Memory | Storage::File(_) | Storage::Directory(_), _) => First::Fresh,
    };
    Ok((storage, first))
}

/// The session `--resume` opens, or why there is none to resume.
async fn resume_target(dir: &Path, cwd: Option<&Path>, id: Option<&str>) -> Result<First> {
    let list = session_files::list(dir).await?;
    match id {
        Some(id) => {
            let session = list
                .find(id)
                .with_context(|| format!("--resume: no single session to resume in {}", dir.display()))?;
            Ok(First::Resume(session.path.clone()))
        }
        None => {
            let cwd = cli::resolve_cwd(cwd)?;
            Ok(match list.latest_in(&cwd) {
                Some(session) => First::Resume(session.path.clone()),
                None => First::NothingToResume(format!(
                    "[chat] no earlier session of this directory in {}; starting a new one",
                    visible(&dir.display().to_string())
                )),
            })
        }
    }
}

/// For /new with `--session`: a new file beside the original.
pub(super) fn new_file(original: &Path) -> PathBuf {
    let mut name = original
        .file_stem()
        .unwrap_or_else(|| OsStr::new("session"))
        .to_os_string();
    name.push(format!(".{}.jsonl", uuid::Uuid::new_v4()));
    original.with_file_name(name)
}
