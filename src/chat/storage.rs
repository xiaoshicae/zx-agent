//! Where a chat keeps its conversations, which session it opens first, and the commands that
//! list, switch and copy sessions.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use zx_agent::AgentSession;

use super::info::About;
use super::output::Output;
use super::term::Tone;
use crate::cli::{self, ChatArgs};
use crate::render::visible;
use crate::sessions::{self, Forked};

/// Where conversations are kept.
pub(super) enum Storage {
    /// `--no-session`.
    Memory,
    /// `--session`: the file named; /new creates one beside it.
    File(PathBuf),
    /// A session directory: every new conversation gets its own file there.
    Directory(PathBuf),
}

impl Storage {
    /// The directory /sessions lists and /resume searches.
    pub(super) fn dir(&self) -> Option<&Path> {
        match self {
            Self::Memory => None,
            Self::File(path) => Some(
                path.parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or(Path::new(".")),
            ),
            Self::Directory(dir) => Some(dir),
        }
    }
}

/// The session a conversation opens with: what `--resume` or `--fork` led to.
pub(super) enum First {
    /// The session `--resume` named, or the latest one of this directory.
    Resume(PathBuf),
    /// The copy `--fork` made.
    Fork(Forked),
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
            Self::Fork(forked) => Some(&forked.path),
            Self::Fresh | Self::NothingToResume(_) => None,
        }
    }

    /// A line to show about the choice, once the conversation is built.
    pub(super) fn notice(&self) -> Option<String> {
        match self {
            Self::Fork(forked) => Some(forked_line(forked)),
            Self::NothingToResume(line) => Some(line.clone()),
            Self::Resume(_) | Self::Fresh => None,
        }
    }
}

/// Where the conversations are kept, and the session to open first.
pub(super) async fn opening(args: &ChatArgs, model: &str) -> Result<(Storage, First)> {
    let storage = if args.no_session {
        Storage::Memory
    } else if let Some(path) = &args.base.session {
        Storage::File(path.clone())
    } else {
        Storage::Directory(sessions::default_dir(&cli::resolve_cwd(args.base.cwd.as_deref())?)?)
    };
    let first = match (&storage, &args.resume, &args.fork) {
        (Storage::Directory(dir), _, Some(source)) => {
            let cwd = cli::resolve_cwd(args.base.cwd.as_deref())?;
            let source = fork_source(dir, source).await?;
            First::Fork(sessions::fork(&source, dir, &cwd, model, sessions::Branch::Last).await?)
        }
        (Storage::Directory(dir), Some(id), None) => {
            resume_target(dir, args.base.cwd.as_deref(), id.as_deref()).await?
        }
        (Storage::Memory | Storage::File(_) | Storage::Directory(_), _, _) => First::Fresh,
    };
    Ok((storage, first))
}

/// The session `--resume` opens, or why there is none to resume.
async fn resume_target(dir: &Path, cwd: Option<&Path>, id: Option<&str>) -> Result<First> {
    let list = sessions::list(dir).await?;
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

/// The session file `--fork` copies: a file, or else the session whose ID starts with
/// `value` in the session directory.
async fn fork_source(dir: &Path, value: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    let list = sessions::list(dir).await?;
    let session = list.find(value).with_context(|| {
        format!(
            "--fork: {value} is neither a session file nor a session in {}",
            dir.display()
        )
    })?;
    Ok(session.path.clone())
}

/// /fork: copy the current session, up to its current position, into a new file of the
/// session directory.
pub(super) async fn fork_current(
    storage: &Storage,
    about: &About,
    model: &str,
    session: &AgentSession,
) -> Result<Forked> {
    let (Some(dir), Some(current)) = (storage.dir(), about.session.as_deref()) else {
        anyhow::bail!("nothing to fork: the conversation is kept in memory");
    };
    // The position /branch moved is only in memory; the file's last record may be elsewhere.
    let leaf = session.inspect(|store| store.leaf_id().map(str::to_string))?;
    sessions::fork(current, dir, &about.cwd, model, sessions::Branch::At(leaf.as_deref())).await
}

pub(super) fn forked_line(forked: &Forked) -> String {
    let short = sessions::short_id(&forked.source_id);
    format!(
        "[chat] forked session {} ({} record(s)) into {}",
        visible(&short),
        forked.entries,
        visible(&forked.path.display().to_string())
    )
}

/// /sessions: the sessions of the session directory, the current one marked with `*`.
pub(super) async fn list_sessions(output: &Output, storage: &Storage, about: &About) {
    let Some(dir) = storage.dir() else {
        output.line(
            Tone::Warn,
            "[chat] no session directory: the conversation is kept in memory",
        );
        return;
    };
    let list = match sessions::list(dir).await {
        Ok(list) => list,
        Err(error) => {
            output.line(Tone::Warn, &format!("[chat] {}", visible(&format!("{error:#}"))));
            return;
        }
    };
    let lines = sessions::lines(&list, &about.cwd);
    for (session, line) in list.sessions.iter().zip(lines) {
        let mark = if about.session.as_deref() == Some(session.path.as_path()) {
            "* "
        } else {
            "  "
        };
        output.line(Tone::Plain, &format!("{mark}{line}"));
    }
    for unreadable in &list.unreadable {
        output.line(
            Tone::Warn,
            &format!(
                "[chat] cannot read {}: {}",
                visible(&unreadable.path.display().to_string()),
                visible(&unreadable.error)
            ),
        );
    }
    output.line(
        Tone::Dim,
        &format!(
            "[chat] {} session(s) in {}; /resume <ID> switches",
            list.sessions.len(),
            visible(&dir.display().to_string())
        ),
    );
}

/// The file of the session /resume names, unless it is the current one.
pub(super) async fn find_session(storage: &Storage, id: &str, about: &About) -> Result<PathBuf> {
    let dir = storage
        .dir()
        .context("no session directory: the conversation is kept in memory")?;
    let list = sessions::list(dir).await?;
    let path = list.find(id)?.path.clone();
    anyhow::ensure!(
        about.session.as_deref() != Some(path.as_path()),
        "already in session {id}"
    );
    Ok(path)
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
