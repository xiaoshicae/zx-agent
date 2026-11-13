//! Where every mode keeps its sessions, and how a listing shows them.
//!
//! As in pi (`getDefaultSessionDirPath`), each working directory has its own directory of
//! session files under the zx-agent home directory, which `ZX_AGENT_HOME` moves.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use zx_agent::config;
use zx_agent::transcript::{
    JsonlStorage, MemoryStorage, SessionHeader, SessionInfo, SessionList, SessionQuery, SessionStorage, Transcript,
};

use crate::render::{local_time, visible};

/// Under the zx-agent home directory: the sessions of every working directory.
const SESSIONS_DIR: &str = "sessions";
/// Characters of a first prompt shown in a listing.
const PROMPT_PREVIEW_CHARS: usize = 60;
/// Characters of a session ID shown in a listing; enough to tell sessions apart, and what
/// `--resume` accepts as a prefix.
const SHORT_ID_CHARS: usize = 8;
/// How a listing shows when a session was last active.
const LISTING_TIME_FORMAT: &str = "%Y-%m-%d %H:%M";

/// Where a mode keeps its sessions.
pub(crate) enum SessionPlace {
    /// `--no-session`: in memory, gone with the process.
    Memory(MemoryStorage),
    /// `--session`: one file, which holds one session.
    File(JsonlStorage),
    /// Session files under one directory, with a directory of its own for each working
    /// directory.
    Directory(JsonlStorage),
}

/// Which session of a place a mode runs on.
pub(crate) enum SessionChoice<'a> {
    /// A new session, with these labels.
    New(BTreeMap<String, String>),
    /// The session with this ID.
    Existing(&'a str),
}

impl<'a> SessionChoice<'a> {
    /// The session `id`, or a new one without labels when there is none to continue.
    pub(crate) fn existing_or_new(id: Option<&'a str>) -> Self {
        id.map_or_else(|| Self::New(BTreeMap::new()), Self::Existing)
    }
}

impl SessionPlace {
    pub(crate) fn memory() -> Self {
        Self::Memory(MemoryStorage::default())
    }

    pub(crate) fn file(path: &Path) -> Self {
        Self::File(JsonlStorage::file(path))
    }

    pub(crate) fn directory(root: &Path) -> Self {
        Self::Directory(JsonlStorage::in_directory(root))
    }

    /// `--session`'s file, or memory without one.
    pub(crate) fn file_or_memory(path: Option<&Path>) -> Self {
        path.map_or_else(Self::memory, Self::file)
    }

    /// `sessions` in the zx-agent home directory (`ZX_AGENT_HOME`, else `~/.zx-agent`).
    pub(crate) fn home() -> Result<Self> {
        let home = config::home()?.context(
            "cannot find the home directory for session files; set ZX_AGENT_HOME, or give --session or --no-session",
        )?;
        Ok(Self::directory(&home.join(SESSIONS_DIR)))
    }

    fn storage(&self) -> &dyn SessionStorage {
        match self {
            Self::Memory(storage) => storage,
            Self::File(storage) | Self::Directory(storage) => storage,
        }
    }

    fn files(&self) -> Option<&JsonlStorage> {
        match self {
            Self::Memory(_) => None,
            Self::File(storage) | Self::Directory(storage) => Some(storage),
        }
    }

    /// Where the sessions started in `cwd` are kept: the session file, or the directory of
    /// that working directory's files; `None` in memory.
    pub(crate) fn location(&self, cwd: &Path) -> Option<PathBuf> {
        self.files().map(|files| files.location(cwd))
    }

    /// The file of session `id`; `None` in memory.
    pub(crate) async fn path_of(&self, id: &str) -> Result<Option<PathBuf>> {
        match self.files() {
            Some(files) => files.path_of(id).await,
            None => Ok(None),
        }
    }

    /// The sessions `query` asks for, most recently active first.
    pub(crate) async fn list(&self, query: &SessionQuery) -> Result<SessionList> {
        self.storage().list(query).await
    }

    /// The session a single file holds already; `None` for a file that does not exist yet,
    /// and for a place that keeps more than one session. A file that cannot be read as a
    /// session is an error.
    pub(crate) async fn only(&self) -> Result<Option<String>> {
        let Self::File(storage) = self else {
            return Ok(None);
        };
        let list = storage.list(&SessionQuery::default()).await?;
        if let Some(unreadable) = list.unreadable.first() {
            bail!("{}", unreadable.error);
        }
        Ok(list.sessions.into_iter().next().map(|session| session.id))
    }

    /// A new session with `header`.
    pub(crate) async fn create(&self, header: SessionHeader) -> Result<Transcript> {
        let log = self.storage().create(&header).await?;
        Ok(Transcript::with_log(header, log))
    }

    /// Session `id`, loaded and open for appending. A record that cannot be loaded names
    /// the session's file.
    pub(crate) async fn open(&self, id: &str) -> Result<Transcript> {
        let (loaded, log) = self.storage().open(id).await?;
        match Transcript::load(loaded, log) {
            Ok(session) => Ok(session),
            Err(error) => Err(match self.path_of(id).await? {
                Some(path) => anyhow!("{}: {error:#}", path.display()),
                None => error.context(format!("cannot load session {id}")),
            }),
        }
    }
}

/// One line per session: short ID, last activity in local time, message count and the
/// session's name, else its first prompt.
pub(crate) fn lines(list: &SessionList) -> Vec<String> {
    list.sessions
        .iter()
        .map(|session| {
            format!(
                "{}  {}  {:>3} message(s)  {}",
                short_id(&session.id),
                local_time(session.last_activity_ms, LISTING_TIME_FORMAT).unwrap_or_else(|| "-".to_string()),
                session.messages,
                preview(session)
            )
        })
        .collect()
}

/// The first [`SHORT_ID_CHARS`] of a session ID, as listings show it.
fn short_id(id: &str) -> String {
    id.chars().take(SHORT_ID_CHARS).collect()
}

fn preview(session: &SessionInfo) -> String {
    match session.title() {
        Some(title) => preview_text(title),
        None => "(no prompt)".to_string(),
    }
}

/// The first non-empty line of `text`, at most [`PROMPT_PREVIEW_CHARS`] characters, with an
/// ellipsis for what is left out and control characters made visible.
pub(crate) fn preview_text(text: &str) -> String {
    let line = text.lines().find(|line| !line.trim().is_empty()).unwrap_or("").trim();
    let mut shown: String = line.chars().take(PROMPT_PREVIEW_CHARS).collect();
    if line.chars().count() > PROMPT_PREVIEW_CHARS || text.trim().lines().count() > 1 {
        shown.push('…');
    }
    visible(&shown)
}

#[cfg(test)]
mod tests {
    use zx_agent::ai::Message;
    use zx_agent::transcript::SessionName;

    use super::*;

    #[test]
    fn a_preview_shows_the_first_line_and_marks_what_it_leaves_out() {
        let session = |prompt: Option<&str>| SessionInfo {
            id: String::new(),
            cwd: String::new(),
            started_ms: 0,
            last_activity_ms: 0,
            messages: 0,
            first_prompt: prompt.map(str::to_string),
            name: None,
            labels: BTreeMap::new(),
        };
        assert_eq!(preview(&session(Some("fix the build"))), "fix the build");
        assert_eq!(preview(&session(Some("\nfirst\nsecond"))), "first…");
        assert_eq!(
            preview(&session(Some(&"界".repeat(61)))),
            format!("{}…", "界".repeat(60))
        );
        assert_eq!(preview(&session(None)), "(no prompt)");
        let named = SessionInfo {
            name: SessionName::new(&"名".repeat(61)),
            ..session(Some("fix the build"))
        };
        assert_eq!(preview(&named), format!("{}…", "名".repeat(60)));
    }

    #[tokio::test]
    async fn a_single_file_holds_its_session_once_it_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");
        let place = SessionPlace::file(&path);
        assert_eq!(place.only().await.unwrap(), None);
        assert_eq!(place.location(Path::new("/w")), Some(path.clone()));

        let header = SessionHeader::new(Path::new("/w"), "m");
        let mut session = place.create(header.clone()).await.unwrap();
        session.append_message(Message::user("q")).await.unwrap();
        drop(session);

        assert_eq!(place.only().await.unwrap(), Some(header.id.clone()));
        assert_eq!(place.path_of(&header.id).await.unwrap(), Some(path));
        assert_eq!(place.open(&header.id).await.unwrap().messages().len(), 1);
    }

    #[tokio::test]
    async fn a_file_that_is_not_a_session_is_an_error_that_names_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");
        std::fs::write(&path, "not json\n").unwrap();

        let error = SessionPlace::file(&path).only().await.unwrap_err();

        let text = format!("{error:#}");
        assert!(
            text.contains("invalid session header") && text.contains("run.jsonl"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_record_that_cannot_be_loaded_names_the_session_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");
        let place = SessionPlace::file(&path);
        let header = SessionHeader::new(Path::new("/w"), "m");
        drop(place.create(header.clone()).await.unwrap());
        let orphan = r#"{"type":"message","id":"a","parentId":"missing","timestamp":1,"message":{"role":"user","content":"q","timestamp":1}}"#;
        std::fs::write(&path, format!("{}{orphan}\n", std::fs::read_to_string(&path).unwrap())).unwrap();

        let error = place.open(&header.id).await.err().unwrap().to_string();

        assert!(error.starts_with(&format!("{}: entry 1: ", path.display())), "{error}");
    }

    #[tokio::test]
    async fn memory_has_no_location() {
        let place = SessionPlace::memory();
        let header = SessionHeader::new(Path::new("/w"), "m");
        drop(place.create(header.clone()).await.unwrap());

        assert_eq!(place.location(Path::new("/w")), None);
        assert_eq!(place.path_of(&header.id).await.unwrap(), None);
        assert_eq!(place.only().await.unwrap(), None);
        assert_eq!(place.list(&SessionQuery::default()).await.unwrap().sessions.len(), 1);
    }
}
