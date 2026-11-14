//! Sessions as JSONL files: one file per session, in a directory per working directory, or
//! one named file (pi: `getDefaultSessionDirPath`, and the names of its session files).
//!
//! - `file`: one session file: its lock, reading it, repairing its end, and appending to it

mod file;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::fs::DirEntry;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow, bail, ensure};
use async_trait::async_trait;

use super::{
    LoadedSession, SessionHeader, SessionInfo, SessionList, SessionLog, SessionQuery, SessionStorage, Transcript,
    Unreadable, ensure_current_version,
};
use crate::sync::lock;
pub(super) use file::JsonlFile;

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
    /// The headers read so far, by file. A header never changes once it is written, so a
    /// file is read for it once; which files exist is asked of the directory every time.
    headers: Mutex<HashMap<PathBuf, SessionHeader>>,
    /// What a listing shows of each session read so far, by file, with what the file looked
    /// like then. A session file only grows, so one that looks the same is not read again.
    listed: Mutex<HashMap<PathBuf, (Stamp, SessionInfo)>>,
}

/// What tells whether a session file changed since it was read: its length, which every
/// record adds to, and when it was last written.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    bytes: u64,
    modified: Option<SystemTime>,
}

impl Stamp {
    fn of(path: &Path) -> Result<Self> {
        let metadata = std::fs::metadata(path).with_context(|| format!("cannot inspect {}", path.display()))?;
        Ok(Self {
            bytes: metadata.len(),
            // Not every platform tells; the length alone still shows every append.
            modified: metadata.modified().ok(),
        })
    }
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
            files: Arc::new(Files {
                layout,
                headers: Mutex::default(),
                listed: Mutex::default(),
            }),
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

    async fn list(&self, query: &SessionQuery) -> Result<SessionList> {
        let query = query.clone();
        self.blocking(move |files| files.list(&query)).await
    }
}

impl Files {
    /// Create the file of a new session. In a directory, fails when a file there is named
    /// after the same session ID; one placed there under another name is not noticed.
    fn create(&self, header: &SessionHeader) -> Result<JsonlFile> {
        ensure_current_version(header)?;
        let path = match &self.layout {
            Layout::File(path) => path.clone(),
            Layout::Directory(root) => {
                let name = file_name(header)?;
                if let Some(existing) = named_after(root, &header.id)? {
                    bail!("session {} exists already: {}", header.id, existing.display());
                }
                let dir = root.join(directory_name(&header.cwd));
                create_private(&dir)?;
                dir.join(name)
            }
        };
        let file = JsonlFile::create(&path, header)?;
        lock(&self.headers).insert(path, header.clone());
        Ok(file)
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
        let mut unreadable = Vec::new();
        for path in self.candidates(query)? {
            match self.listed(&path, query) {
                Ok(Some(session)) => sessions.push(session),
                Ok(None) => {}
                Err(error) => unreadable.push(Unreadable {
                    location: path.display().to_string(),
                    error: format!("{error:#}"),
                }),
            }
        }
        unreadable.sort_by(|a, b| a.location.cmp(&b.location));
        Ok(SessionList {
            sessions: query.arrange(sessions),
            unreadable,
        })
    }

    /// The files in which `query` may find its sessions.
    fn candidates(&self, query: &SessionQuery) -> Result<Vec<PathBuf>> {
        match &self.layout {
            Layout::File(path) => Ok(if path.is_file() { vec![path.clone()] } else { Vec::new() }),
            Layout::Directory(root) => {
                let directories = match &query.cwd {
                    Some(cwd) => vec![root.join(directory_name(&cwd.display().to_string()))],
                    None => directories_in(root)?,
                };
                let mut files = Vec::new();
                for directory in directories {
                    files.extend(session_files_in(&directory)?);
                }
                Ok(files)
            }
        }
    }

    /// What a listing shows of the session in `path`, if `query` asks for it. Only a
    /// session it asks for is read whole, and only when its file changed since it was last
    /// read for a listing.
    fn listed(&self, path: &Path, query: &SessionQuery) -> Result<Option<SessionInfo>> {
        if !query.matches(&self.header_of(path)?) {
            return Ok(None);
        }
        // Looked at before the file is read: a record written meanwhile changes how the
        // file looks next time, and it is read again then.
        let stamp = Stamp::of(path)?;
        if let Some((seen, info)) = lock(&self.listed).get(path)
            && *seen == stamp
        {
            return Ok(Some(info.clone()));
        }
        let transcript =
            Transcript::snapshot(file::read(path)?).map_err(|error| anyhow!("{}: {error:#}", path.display()))?;
        let info = SessionInfo::of(&transcript);
        lock(&self.listed).insert(path.to_path_buf(), (stamp, info.clone()));
        Ok(Some(info))
    }

    /// The header of the session file `path`, read once.
    fn header_of(&self, path: &Path) -> Result<SessionHeader> {
        if let Some(header) = lock(&self.headers).get(path) {
            return Ok(header.clone());
        }
        let header = file::read_header(path)?;
        lock(&self.headers).insert(path.to_path_buf(), header.clone());
        Ok(header)
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
                return Ok(if path.is_file() && self.header_of(path)?.id == id {
                    Some(path.clone())
                } else {
                    None
                });
            }
            Layout::Directory(root) => root,
        };
        let known = lock(&self.headers)
            .iter()
            .find(|(_, header)| header.id == id)
            .map(|(path, _)| path.clone());
        if let Some(path) = known.filter(|path| path.is_file()) {
            return Ok(Some(path));
        }
        let mut files = Vec::new();
        for directory in directories_in(root)? {
            files.extend(session_files_in(&directory)?);
        }
        // A file named after the session first, so that finding it reads few headers.
        files.sort_by_key(|path| !is_named_after(path, id));
        // A file that cannot be read is not the session; a listing reports it.
        Ok(files
            .into_iter()
            .find(|path| self.header_of(path).is_ok_and(|header| header.id == id)))
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

/// A session file under `root` that is named after session `id`, by its name alone.
fn named_after(root: &Path, id: &str) -> Result<Option<PathBuf>> {
    for directory in directories_in(root)? {
        if let Some(path) = session_files_in(&directory)?
            .into_iter()
            .find(|path| is_named_after(path, id))
        {
            return Ok(Some(path));
        }
    }
    Ok(None)
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
        .map(|c| if matches!(c, '/' | '\\' | ':') { '-' } else { c })
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
    // Days since the epoch to a date of the Gregorian calendar, counted in 400-year cycles
    // of 146097 days that start on March 1, so that a leap day ends its year (Howard
    // Hinnant, "chrono-Compatible Low-Level Date Algorithms", `civil_from_days`).
    let shifted = days + 719_468;
    let (cycle, day_of_cycle) = (shifted / 146_097, shifted % 146_097);
    let year_of_cycle = (day_of_cycle - day_of_cycle / 1460 + day_of_cycle / 36_524 - day_of_cycle / 146_096) / 365;
    let day_of_year = day_of_cycle - (365 * year_of_cycle + year_of_cycle / 4 - year_of_cycle / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = year_of_cycle + cycle * 400 + u64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}-{minute:02}-{second:02}-{milli:03}Z")
}

/// Create `directory` and its missing parents, readable only by this user on Unix: session
/// files hold the conversation and tool output. Existing directories keep their permissions.
fn create_private(directory: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(directory)
        .with_context(|| format!("cannot create the session directory {}", directory.display()))
}
