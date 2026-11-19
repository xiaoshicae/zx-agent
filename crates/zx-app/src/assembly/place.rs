//! Where a host keeps the sessions of an agent.
//!
//! As in pi (`getDefaultSessionDirPath`), each working directory has its own directory of
//! session files under the zx-agent home directory, which `ZX_AGENT_HOME` moves.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::config;
use zx_agent::builtin::jsonl::JsonlStorage;
use zx_agent::transcript::{MemoryStorage, SessionList, SessionQuery, SessionStorage};

/// Under the zx-agent home directory: the sessions of every working directory.
const SESSIONS_DIR: &str = "sessions";

/// Where a configured agent keeps its sessions. Clones share the same storage; constructors
/// keep its layout private so a single-file storage cannot be confused with a directory.
#[derive(Clone)]
pub struct SessionPlace(Place);

#[derive(Clone)]
enum Place {
    /// In memory. A session is kept while it is open and forgotten once it is closed, so
    /// that a process that runs for long keeps nothing of the conversations it has had.
    Memory(Arc<MemoryStorage>),
    /// One file, which holds one session.
    File(Arc<JsonlStorage>),
    /// Session files under one directory, with a directory of its own for each working
    /// directory.
    Directory(Arc<JsonlStorage>),
    /// The host's own storage, such as a database.
    Storage(Arc<dyn SessionStorage>),
}

impl SessionPlace {
    /// Keep open sessions in memory and forget them when closed.
    pub fn memory() -> Self {
        Self(Place::Memory(Arc::new(MemoryStorage::forgetting())))
    }

    /// Keep at most one session in this JSONL file: the storage holds none while the file
    /// does not exist, and creating a second session fails.
    pub fn file(path: &Path) -> Self {
        Self(Place::File(Arc::new(JsonlStorage::file(path))))
    }

    /// Keep session files under this explicit root, grouped by working directory.
    pub fn directory(root: &Path) -> Self {
        Self(Place::Directory(Arc::new(JsonlStorage::in_directory(root))))
    }

    /// Keep the sessions in `storage`, the host's own, such as a database: closed sessions
    /// stay there to be opened again, and none has a file.
    pub fn from_storage(storage: Arc<dyn SessionStorage>) -> Self {
        Self(Place::Storage(storage))
    }

    /// Whether closed sessions remain and can be opened later: everywhere but in memory.
    pub fn is_persistent(&self) -> bool {
        !matches!(self.0, Place::Memory(_))
    }

    /// `sessions` in the zx-agent home directory (`ZX_AGENT_HOME`, else `~/.zx-agent`).
    /// Fails when no home directory is known.
    pub fn home() -> Result<Self> {
        let home = config::home()?.context("cannot find the home directory for session files; set ZX_AGENT_HOME")?;
        Ok(Self::directory(&home.join(SESSIONS_DIR)))
    }

    /// The storage of this place, for the agent whose sessions are kept here, and for a host
    /// that lists them.
    pub fn storage(&self) -> Arc<dyn SessionStorage> {
        match &self.0 {
            Place::Memory(storage) => storage.clone(),
            Place::File(storage) | Place::Directory(storage) => storage.clone(),
            Place::Storage(storage) => Arc::clone(storage),
        }
    }

    fn files(&self) -> Option<&JsonlStorage> {
        match &self.0 {
            Place::Memory(_) | Place::Storage(_) => None,
            Place::File(storage) | Place::Directory(storage) => Some(storage),
        }
    }

    /// Where the sessions started in `cwd` are kept: the session file, or the directory of
    /// that working directory's files; `None` without files.
    pub fn location(&self, cwd: &Path) -> Option<PathBuf> {
        self.files().map(|files| files.location(cwd))
    }

    /// The file of session `id`; `None` without files.
    pub async fn path_of(&self, id: &str) -> Result<Option<PathBuf>> {
        match self.files() {
            Some(files) => files.path_of(id).await,
            None => Ok(None),
        }
    }

    /// The sessions kept here that were started in `cwd`, most recently active first, and
    /// what was found among them and could not be read.
    pub async fn started_in(&self, cwd: &Path) -> Result<SessionList> {
        let query = SessionQuery {
            cwd: Some(cwd.to_path_buf()),
            ..SessionQuery::default()
        };
        self.storage().list(&query).await
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
    async fn a_single_file_is_where_its_session_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");
        let place = SessionPlace::file(&path);
        assert_eq!(place.location(Path::new("/w")), Some(path.clone()));

        let header = SessionHeader::new(Path::new("/w"), "m");
        drop(create(&place, &header).await);

        assert_eq!(place.path_of(&header.id).await.unwrap(), Some(path));
    }

    #[tokio::test]
    async fn an_error_of_opening_a_session_names_the_file_the_session_has() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");
        let place = SessionPlace::file(&path);
        let header = SessionHeader::new(Path::new("/w"), "m");
        drop(create(&place, &header).await);

        let error = place
            .naming_file(&header.id, anyhow::anyhow!("record 1: no parent"))
            .await;
        assert_eq!(
            format!("{error:#}"),
            format!("cannot open the session in {}: record 1: no parent", path.display())
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
        assert_eq!(place.started_in(Path::new("/w")).await.unwrap().sessions.len(), 1);
    }

    #[tokio::test]
    async fn the_hosts_own_storage_keeps_the_sessions_and_has_no_files() {
        let storage = Arc::new(MemoryStorage::default());
        let place = SessionPlace::from_storage(storage.clone());
        let header = SessionHeader::new(Path::new("/w"), "m");
        drop(create(&place, &header).await);

        assert!(place.is_persistent());
        assert_eq!(place.location(Path::new("/w")), None);
        assert_eq!(place.path_of(&header.id).await.unwrap(), None);
        let kept = storage.list(&SessionQuery::default()).await.unwrap();
        assert_eq!(kept.sessions.len(), 1);
        assert_eq!(place.started_in(Path::new("/w")).await.unwrap().sessions.len(), 1);
    }

    #[tokio::test]
    async fn memory_keeps_nothing_of_a_session_that_was_closed() {
        // What a host that runs for long relies on when its clients close their sessions.
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
