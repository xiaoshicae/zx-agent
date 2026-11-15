//! Where a mode keeps its sessions.
//!
//! As in pi (`getDefaultSessionDirPath`), each working directory has its own directory of
//! session files under the zx-agent home directory, which `ZX_AGENT_HOME` moves.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use crate::config;
use zx_agent::transcript::{JsonlStorage, MemoryStorage, SessionInfo, SessionList, SessionQuery, SessionStorage};

/// Under the zx-agent home directory: the sessions of every working directory.
const SESSIONS_DIR: &str = "sessions";

/// Where a mode keeps its sessions. A clone is the same place: the mode's agent keeps its
/// sessions there, and the mode lists them and shows their files.
#[derive(Clone)]
pub(crate) enum SessionPlace {
    /// `--no-session`: in memory. A session is kept while it is open and forgotten once it
    /// is closed, so that a process that runs for long keeps nothing of the conversations it
    /// has had.
    Memory(Arc<MemoryStorage>),
    /// `--session`: one file, which holds one session.
    File(Arc<JsonlStorage>),
    /// Session files under one directory, with a directory of its own for each working
    /// directory.
    Directory(Arc<JsonlStorage>),
}

impl SessionPlace {
    pub(crate) fn memory() -> Self {
        Self::Memory(Arc::new(MemoryStorage::forgetting()))
    }

    pub(crate) fn file(path: &Path) -> Self {
        Self::File(Arc::new(JsonlStorage::file(path)))
    }

    pub(crate) fn directory(root: &Path) -> Self {
        Self::Directory(Arc::new(JsonlStorage::in_directory(root)))
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

    /// The storage of this place, for the agent whose sessions are kept here.
    pub(crate) fn storage(&self) -> Arc<dyn SessionStorage> {
        match self {
            Self::Memory(storage) => storage.clone(),
            Self::File(storage) | Self::Directory(storage) => storage.clone(),
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

    /// The sessions kept here that were started in `cwd`, most recently active first, and
    /// what was found among them and could not be read.
    pub(crate) async fn started_in(&self, cwd: &Path) -> Result<SessionList> {
        let query = SessionQuery {
            cwd: Some(cwd.to_path_buf()),
            ..SessionQuery::default()
        };
        self.storage().list(&query).await
    }

    /// The session a single file holds already; `None` for a file that does not exist yet,
    /// and for a place that keeps more than one session. A file that cannot be read as a
    /// session is an error.
    pub(crate) async fn only(&self) -> Result<Option<SessionInfo>> {
        let Self::File(storage) = self else {
            return Ok(None);
        };
        let list = storage.list(&SessionQuery::default()).await?;
        if let Some(unreadable) = list.unreadable.first() {
            bail!("{}", unreadable.error);
        }
        Ok(list.sessions.into_iter().next())
    }

    /// `error`, which opening session `id` gave, naming the session's file when it has one.
    pub(crate) async fn naming_file(&self, id: &str, error: anyhow::Error) -> anyhow::Error {
        // A path that cannot be found now adds nothing to the error at hand.
        match self.path_of(id).await.ok().flatten() {
            Some(path) => error.context(format!("cannot open the session in {}", path.display())),
            None => error,
        }
    }
}

#[cfg(test)]
mod tests {
    use zx_agent::ai::Message;
    use zx_agent::transcript::{SessionHeader, Transcript};

    use super::*;

    /// A new session with `header` in `place`, open for appending.
    async fn create(place: &SessionPlace, header: &SessionHeader) -> Transcript {
        Transcript::with_log(header.clone(), place.storage().create(header).await.unwrap())
    }

    #[tokio::test]
    async fn a_single_file_holds_its_session_once_it_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");
        let place = SessionPlace::file(&path);
        assert_eq!(place.only().await.unwrap(), None);
        assert_eq!(place.location(Path::new("/w")), Some(path.clone()));

        let header = SessionHeader::new(Path::new("/w"), "m");
        let mut session = create(&place, &header).await;
        session.append_message(Message::user("q")).await.unwrap();
        drop(session);

        let held = place.only().await.unwrap().unwrap();
        assert_eq!(
            (held.id.as_str(), held.cwd.as_str(), held.messages),
            (header.id.as_str(), "/w", 1)
        );
        assert_eq!(place.path_of(&header.id).await.unwrap(), Some(path));
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
    async fn an_error_of_opening_a_session_names_the_file_the_session_has() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");
        let place = SessionPlace::file(&path);
        let header = SessionHeader::new(Path::new("/w"), "m");
        drop(create(&place, &header).await);

        let error = place
            .naming_file(&header.id, anyhow::anyhow!("entry 1: no parent"))
            .await;
        assert_eq!(
            format!("{error:#}"),
            format!("cannot open the session in {}: entry 1: no parent", path.display())
        );

        let kept_in_memory = SessionPlace::memory()
            .naming_file(&header.id, anyhow::anyhow!("no such session"))
            .await;
        assert_eq!(format!("{kept_in_memory:#}"), "no such session");
    }

    #[tokio::test]
    async fn memory_has_no_location() {
        let place = SessionPlace::memory();
        let header = SessionHeader::new(Path::new("/w"), "m");
        let _open = create(&place, &header).await;

        assert_eq!(place.location(Path::new("/w")), None);
        assert_eq!(place.path_of(&header.id).await.unwrap(), None);
        assert_eq!(place.only().await.unwrap(), None);
        assert_eq!(place.started_in(Path::new("/w")).await.unwrap().sessions.len(), 1);
    }

    #[tokio::test]
    async fn memory_keeps_nothing_of_a_session_that_was_closed() {
        // What `acp --no-session` relies on: it runs for long, and its client closes sessions.
        let place = SessionPlace::memory();
        let header = SessionHeader::new(Path::new("/w"), "m");
        let mut open = create(&place, &header).await;
        open.append_message(Message::user("q")).await.unwrap();

        drop(open);

        assert!(place.started_in(Path::new("/w")).await.unwrap().sessions.is_empty());
        assert!(place.storage().open(&header.id).await.is_err());
    }

    #[tokio::test]
    async fn the_sessions_started_in_a_directory_are_those_whose_header_names_it() {
        let dir = tempfile::tempdir().unwrap();
        let place = SessionPlace::directory(dir.path());
        let here = SessionHeader::new(Path::new("/w"), "m");
        let elsewhere = SessionHeader::new(Path::new("/w/nested"), "m");
        drop(create(&place, &here).await);
        drop(create(&place, &elsewhere).await);

        let started = |cwd: &'static str| async {
            let list = place.started_in(Path::new(cwd)).await.unwrap();
            list.sessions.into_iter().map(|session| session.id).collect::<Vec<_>>()
        };

        assert_eq!(started("/w").await, [here.id.as_str()]);
        assert_eq!(started("/w/nested").await, [elsewhere.id.as_str()]);
        assert!(started("/other").await.is_empty());
    }
}
