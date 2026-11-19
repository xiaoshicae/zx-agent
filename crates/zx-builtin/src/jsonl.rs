//! Sessions as JSONL files: one file per session, in a directory per working directory, or
//! one named file (pi: `getDefaultSessionDirPath`, and the names of its session files).
//!
//! - `file`: one session file: its lock, reading it, repairing its end, and appending to it
//! - `test_support` and `tests`, compiled for tests only: what the tests of session files
//!   share, and the tests of the storage

mod file;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

use std::fs::DirEntry;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail, ensure};
use async_trait::async_trait;

use zx_core::transcript::{
    LoadedSession, SessionHeader, SessionInfo, SessionList, SessionLog, SessionQuery, SessionStorage, Transcript,
    Unreadable,
};

use crate::calendar::civil_from_days;
use file::JsonlFile;

/// What session files end in.
const EXTENSION: &str = "jsonl";

/// Sessions kept as JSONL files: a header line, then one record per line.
///
/// On Unix a session's file holds an exclusive `flock(2)` lock while a log of the session
/// exists, so opening the session again, in this process or another, fails until that log is
/// dropped; other platforms do not detect a second writer. A new file is readable only by
/// its owner. A crash during the final write can leave an incomplete last line: it is left
/// out when the session is opened, and cut off before the next record is appended, once the
/// session's other records have been checked.
///
/// The storage reads, creates and locks files on Tokio's blocking threads, so it needs a
/// Tokio runtime.
pub struct JsonlStorage {
    /// Shared with the blocking threads that do the file work.
    files: Arc<Files>,
}

/// Where the session files are.
enum Layout {
    /// `<root>/--<cwd>--/<UTC time>_<session ID>.jsonl`: a directory per working directory.
    Directory(PathBuf),
    /// One file, which holds at most one session.
    File(PathBuf),
}

struct Files {
    layout: Layout,
}

impl JsonlStorage {
    /// Sessions kept under `root`. Each working directory has a directory of its own there,
    /// `--<cwd>--` with the separators of `cwd` turned into `-`, and each session a file in
    /// it, `<UTC time>_<session ID>.jsonl`, so that the names sort in the order the sessions
    /// were started. A session ID may hold ASCII letters, digits, `-` and `_`.
    ///
    /// Directories are created with the first session that needs them, readable only by this
    /// user on Unix. Files found there under other names are sessions too.
    pub fn in_directory(root: impl Into<PathBuf>) -> Self {
        Self::new(Layout::Directory(root.into()))
    }

    /// One session file, `path`: a storage that holds the session in that file, or none
    /// while the file does not exist. Creating a second session fails.
    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self::new(Layout::File(path.into()))
    }

    fn new(layout: Layout) -> Self {
        Self {
            files: Arc::new(Files { layout }),
        }
    }

    /// Where the sessions started in `cwd` are kept: their directory, or the storage's one
    /// file.
    pub fn location(&self, cwd: &Path) -> PathBuf {
        match &self.files.layout {
            Layout::Directory(root) => root.join(directory_name(&cwd.display().to_string())),
            Layout::File(path) => path.clone(),
        }
    }

    /// The file that holds session `id`; `None` when the storage holds no such session.
    pub async fn path_of(&self, id: &str) -> Result<Option<PathBuf>> {
        let id = id.to_string();
        self.blocking(move |files| files.find(&id)).await
    }

    /// Run `work` on a blocking thread: it reads, creates or locks files.
    async fn blocking<T: Send + 'static>(&self, work: impl FnOnce(&Files) -> Result<T> + Send + 'static) -> Result<T> {
        let files = Arc::clone(&self.files);
        tokio::task::spawn_blocking(move || work(&files))
            .await
            .context("reading or writing session files stopped")?
    }
}

#[async_trait]
impl SessionStorage for JsonlStorage {
    async fn create(&self, header: &SessionHeader) -> Result<Box<dyn SessionLog>> {
        let header = header.clone();
        let file = self.blocking(move |files| files.create(&header)).await?;
        Ok(Box::new(file))
    }

    async fn open(&self, id: &str) -> Result<(LoadedSession, Box<dyn SessionLog>)> {
        let id = id.to_string();
        let (loaded, file) = self.blocking(move |files| files.open(&id)).await?;
        Ok((loaded, Box::new(file)))
    }

    async fn read(&self, id: &str) -> Result<LoadedSession> {
        let id = id.to_string();
        self.blocking(move |files| files.read(&id)).await
    }

    async fn headers(&self, query: &SessionQuery) -> Result<Vec<SessionHeader>> {
        let query = query.clone();
        self.blocking(move |files| files.headers(&query)).await
    }

    async fn list(&self, query: &SessionQuery) -> Result<SessionList> {
        let query = query.clone();
        self.blocking(move |files| files.list(&query)).await
    }
}

impl Files {
    /// Create the file of a new session. In a directory, fails when a file there is named
    /// after the same session ID; one placed there under another name is not noticed.
    fn create(&self, header: &SessionHeader) -> Result<JsonlFile> {
        header.check_version()?;
        let path = match &self.layout {
            Layout::File(path) => path.clone(),
            Layout::Directory(root) => {
                let name = file_name(header)?;
                if let Some(existing) = named_after(root, &header.id)? {
                    bail!("session {} exists already: {}", header.id, existing.display());
                }
                root.join(directory_name(&header.cwd)).join(name)
            }
        };
        JsonlFile::create(&path, header)
    }

    fn open(&self, id: &str) -> Result<(LoadedSession, JsonlFile)> {
        let path = self.existing(id)?;
        let (loaded, file) = JsonlFile::open(&path)?;
        holds(&path, &loaded.header, id)?;
        Ok((loaded, file))
    }

    fn read(&self, id: &str) -> Result<LoadedSession> {
        let path = self.existing(id)?;
        let loaded = file::read(&path)?;
        holds(&path, &loaded.header, id)?;
        Ok(loaded)
    }

    fn list(&self, query: &SessionQuery) -> Result<SessionList> {
        let mut sessions = Vec::new();
        let (candidates, mut unreadable) = self.candidates(query)?;
        for path in candidates {
            match listed(&path, query) {
                Ok(Some(session)) => sessions.push(session),
                Ok(None) => {}
                Err(error) => unreadable.push(Unreadable {
                    location: path.display().to_string(),
                    error: format!("{error:#}"),
                }),
            }
        }
        Ok(SessionList::new(query, sessions, unreadable))
    }

    /// The headers of the sessions `query` asks for, from the first line of each file. A file
    /// whose header cannot be read, or that holds another format's session, is no session
    /// this storage can open, so it is left out; a listing reports it.
    fn headers(&self, query: &SessionQuery) -> Result<Vec<SessionHeader>> {
        let (paths, _) = self.candidates(query)?;
        let headers = paths
            .iter()
            .filter_map(|path| file::read_header(path).ok())
            .filter(|header| header.check_version().is_ok())
            .collect();
        Ok(query.arrange_headers(headers))
    }

    /// The files in which `query` may find its sessions, and the directories it would look in
    /// that cannot be read.
    fn candidates(&self, query: &SessionQuery) -> Result<(Vec<PathBuf>, Vec<Unreadable>)> {
        match &self.layout {
            Layout::File(path) => Ok((if path.is_file() { vec![path.clone()] } else { Vec::new() }, Vec::new())),
            Layout::Directory(root) => {
                let directories = match &query.cwd {
                    Some(cwd) => vec![root.join(directory_name(&cwd.display().to_string()))],
                    None => directories_in(root)?,
                };
                Ok(session_files_in_each(directories))
            }
        }
    }

    /// The file of session `id`, or an error that says where it was looked for.
    fn existing(&self, id: &str) -> Result<PathBuf> {
        self.find(id)?.ok_or_else(|| {
            let (Layout::Directory(place) | Layout::File(place)) = &self.layout;
            anyhow!("no session {id} in {}", place.display())
        })
    }

    /// The file that holds session `id`, if there is one.
    fn find(&self, id: &str) -> Result<Option<PathBuf>> {
        let root = match &self.layout {
            Layout::File(path) => {
                // The one candidate: a file that cannot be read is an error, not an absence.
                return Ok(if path.is_file() && file::read_header(path)?.id == id {
                    Some(path.clone())
                } else {
                    None
                });
            }
            Layout::Directory(root) => root,
        };
        // A directory that cannot be read holds no session this storage can open; a listing
        // reports it.
        let (mut files, _) = session_files_in_each(directories_in(root)?);
        // A file named after the session first, so that finding it reads few headers.
        files.sort_by_key(|path| !is_named_after(path, id));
        // A file that cannot be read is not the session; a listing reports it.
        Ok(files
            .into_iter()
            .find(|path| file::read_header(path).is_ok_and(|header| header.id == id)))
    }
}

/// An error unless the session in `path`, with `header`, is session `id`: the file changed
/// since its header was read.
fn holds(path: &Path, header: &SessionHeader, id: &str) -> Result<()> {
    ensure!(
        header.id == id,
        "{} holds session {}, not {id}",
        path.display(),
        header.id
    );
    Ok(())
}

/// The directories directly in `root`; none when `root` does not exist.
fn directories_in(root: &Path) -> Result<Vec<PathBuf>> {
    Ok(entries_in(root)?
        .into_iter()
        .filter(|entry| is_a(entry, Kind::Directory))
        .map(|entry| entry.path())
        .collect())
}

/// The session files directly in `directory`; none when it does not exist. A directory named
/// like a session is not one; neither is a link that leads nowhere.
fn session_files_in(directory: &Path) -> Result<Vec<PathBuf>> {
    Ok(entries_in(directory)?
        .into_iter()
        .filter(|entry| {
            Path::new(&entry.file_name())
                .extension()
                .is_some_and(|extension| extension == EXTENSION)
        })
        .filter(|entry| is_a(entry, Kind::File))
        .map(|entry| entry.path())
        .collect())
}

/// What a listing shows of the session in `path`, if `query` asks for it. Only a session it
/// asks for is read whole.
fn listed(path: &Path, query: &SessionQuery) -> Result<Option<SessionInfo>> {
    if !query.matches(&file::read_header(path)?) {
        return Ok(None);
    }
    let transcript = Transcript::snapshot(file::read(path)?).with_context(|| path.display().to_string())?;
    Ok(Some(SessionInfo::of(&transcript)))
}

/// The session files in each of `directories`, and the directories that cannot be read.
fn session_files_in_each(directories: Vec<PathBuf>) -> (Vec<PathBuf>, Vec<Unreadable>) {
    let mut files = Vec::new();
    let mut unreadable = Vec::new();
    for directory in directories {
        match session_files_in(&directory) {
            Ok(found) => files.extend(found),
            Err(error) => unreadable.push(Unreadable {
                location: directory.display().to_string(),
                error: format!("{error:#}"),
            }),
        }
    }
    (files, unreadable)
}

/// What `directory` holds; nothing when it does not exist.
fn entries_in(directory: &Path) -> Result<Vec<DirEntry>> {
    let cannot_list = || format!("cannot list sessions in {}", directory.display());
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(cannot_list),
    };
    entries.map(|entry| entry.with_context(cannot_list)).collect()
}

/// What an entry of a directory is asked to be.
#[derive(Clone, Copy)]
enum Kind {
    File,
    Directory,
}

/// Whether `entry` is of `kind`, or a link that leads to one. The directory says what its
/// entries are, so only a link is looked up; an entry that cannot be asked is neither.
fn is_a(entry: &DirEntry, kind: Kind) -> bool {
    match (entry.file_type(), kind) {
        (Ok(found), Kind::File) if found.is_symlink() => entry.path().is_file(),
        (Ok(found), Kind::Directory) if found.is_symlink() => entry.path().is_dir(),
        (Ok(found), Kind::File) => found.is_file(),
        (Ok(found), Kind::Directory) => found.is_dir(),
        (Err(_), _) => false,
    }
}

/// A session file under `root` that is named after session `id`, by its name alone. A
/// directory that cannot be read is passed over, as finding a session passes it over.
fn named_after(root: &Path, id: &str) -> Result<Option<PathBuf>> {
    let (files, _) = session_files_in_each(directories_in(root)?);
    Ok(files.into_iter().find(|path| is_named_after(path, id)))
}

/// Whether the file name of `path` names session `id`: `<id>.jsonl`, or what
/// [`file_name`] gives a session, `<UTC time>_<id>.jsonl`.
fn is_named_after(path: &Path, id: &str) -> bool {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem == id || stem.split_once('_').is_some_and(|(_, named)| named == id))
}

/// `--<cwd>--`, with the separators of `cwd` turned into `-` as pi does, so that one
/// directory holds one working directory's sessions.
fn directory_name(cwd: &str) -> String {
    let encoded: String = cwd
        .strip_prefix(['/', '\\'])
        .unwrap_or(cwd)
        .chars()
        .map(|character| {
            if matches!(character, '/' | '\\' | ':') {
                '-'
            } else {
                character
            }
        })
        .collect();
    format!("--{encoded}--")
}

/// `<UTC time>_<session ID>.jsonl`, as pi names them: sorted by name they are in the order
/// they were started. Fails for an ID that could name another file or directory.
fn file_name(header: &SessionHeader) -> Result<String> {
    let id = &header.id;
    ensure!(
        !id.is_empty()
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
        "session ID {id:?} cannot name a session file: use ASCII letters, digits, `-` and `_`"
    );
    Ok(format!("{}_{id}.{EXTENSION}", utc_time(header.timestamp)))
}

/// `ms` since the Unix epoch as `2025-11-04T23-46-40-123Z`: UTC, with `-` where a file name
/// cannot hold `:`.
fn utc_time(ms: u64) -> String {
    const MS_PER_DAY: u64 = 24 * 60 * 60 * 1000;
    let (days, ms_of_day) = (ms / MS_PER_DAY, ms % MS_PER_DAY);
    let (hour, minute) = (ms_of_day / 3_600_000, ms_of_day / 60_000 % 60);
    let (second, milli) = (ms_of_day / 1000 % 60, ms_of_day % 1000);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}-{minute:02}-{second:02}-{milli:03}Z")
}
