//! Statuses of runs in progress, kept so that the next start can replace those a crash left
//! behind.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::server::{Shared, segment};
use super::{ConversationId, Destination, ImChannel, MessageId, MessageRef, Outbound, SessionKey, ThreadId};

/// The extension of a record file; a record is written under another name first.
const RECORD: &str = "json";
const PARTIAL: &str = "partial";

/// Keeps the status message of each run while it runs: the one that says
/// [`Replies::working`](super::Replies::working).
///
/// The server keeps a status once it is posted and removes it just before the run's answer
/// replaces it. What [`list`](Self::list) returns as the server starts is therefore what an
/// earlier process left behind when it ended without replacing them, by a crash or a kill;
/// the server replaces those with [`Replies::interrupted`](super::Replies::interrupted).
#[async_trait]
pub trait StatusStore: Send + Sync {
    /// Keep `status`, posted as a run of `key` started.
    async fn insert(&self, key: &SessionKey, status: &MessageRef) -> Result<(), String>;

    /// Forget `status`. Forgetting a status that is not kept succeeds.
    async fn remove(&self, key: &SessionKey, status: &MessageRef) -> Result<(), String>;

    /// The statuses kept. The server calls this once, before it receives anything.
    async fn list(&self) -> Result<Vec<(SessionKey, MessageRef)>, String>;
}

/// A [`StatusStore`] with one JSON file per status in a directory.
///
/// A record is written under a temporary name and then renamed, so that a crash never leaves
/// part of one. On Unix only the owner may read a record, as with session files.
pub struct StatusFiles {
    dir: PathBuf,
}

impl StatusFiles {
    /// Keep statuses in `dir`, which must exist and hold nothing else.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// `<platform>.<conversation>.<message>.json`, each part encoded like the parts of a
    /// session directory ([`SessionKey::dir_in`]), which never contain a `.`.
    fn path(&self, key: &SessionKey, status: &MessageRef) -> PathBuf {
        self.dir.join(format!(
            "{}.{}.{}.{RECORD}",
            segment(&key.platform),
            segment(&status.at.conversation.0),
            segment(&status.id.0)
        ))
    }
}

// Small writes, renames and reads of a few files, like a session file's appended line
// (crates/zx-core/src/session.rs); `list` runs once, before the server receives anything.
#[async_trait]
impl StatusStore for StatusFiles {
    async fn insert(&self, key: &SessionKey, status: &MessageRef) -> Result<(), String> {
        let path = self.path(key, status);
        let text = serde_json::to_string(&Record::new(key, status))
            .map_err(|error| format!("cannot encode the record of {}: {error}", path.display()))?;
        let partial = path.with_extension(PARTIAL);
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        // Which chats and threads the server answers in is not for other users to read.
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options
            .open(&partial)
            .and_then(|mut file| file.write_all(text.as_bytes()))
            .map_err(|error| format!("cannot write {}: {error}", partial.display()))?;
        if let Err(error) = std::fs::rename(&partial, &path) {
            // The record failed either way. Should the removal fail too, the file stays, and
            // `list` passes over it.
            std::fs::remove_file(&partial).ok();
            return Err(format!(
                "cannot rename {} to {}: {error}",
                partial.display(),
                path.display()
            ));
        }
        Ok(())
    }

    async fn remove(&self, key: &SessionKey, status: &MessageRef) -> Result<(), String> {
        let path = self.path(key, status);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("cannot remove {}: {error}", path.display())),
        }
    }

    async fn list(&self) -> Result<Vec<(SessionKey, MessageRef)>, String> {
        let unreadable = |path: &Path, error: io::Error| format!("cannot read {}: {error}", path.display());
        let mut paths = Vec::new();
        for entry in std::fs::read_dir(&self.dir).map_err(|error| unreadable(&self.dir, error))? {
            let path = entry.map_err(|error| unreadable(&self.dir, error))?.path();
            // Anything else is a write that a crash cut short before its rename.
            if path.extension().is_some_and(|extension| extension == RECORD) {
                paths.push(path);
            }
        }
        paths.sort();
        let mut kept = Vec::with_capacity(paths.len());
        for path in paths {
            let text = std::fs::read_to_string(&path).map_err(|error| unreadable(&path, error))?;
            let record: Record = serde_json::from_str(&text)
                .map_err(|error| format!("{} is not a status record: {error}", path.display()))?;
            kept.push(record.into_parts());
        }
        Ok(kept)
    }
}

/// One kept status, as stored.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Record {
    platform: String,
    conversation: String,
    /// `None` for a direct chat.
    thread: Option<String>,
    status: StoredStatus,
}

/// The status message as stored: where it was posted, and its ID.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct StoredStatus {
    conversation: String,
    thread: Option<String>,
    id: String,
}

impl Record {
    fn new(key: &SessionKey, status: &MessageRef) -> Self {
        Self {
            platform: key.platform.clone(),
            conversation: key.conversation.0.clone(),
            thread: key.thread.as_ref().map(|thread| thread.0.clone()),
            status: StoredStatus {
                conversation: status.at.conversation.0.clone(),
                thread: status.at.thread.as_ref().map(|thread| thread.0.clone()),
                id: status.id.0.clone(),
            },
        }
    }

    fn into_parts(self) -> (SessionKey, MessageRef) {
        let key = SessionKey {
            platform: self.platform,
            conversation: ConversationId(self.conversation),
            thread: self.thread.map(ThreadId),
        };
        let status = MessageRef {
            at: Destination {
                conversation: ConversationId(self.status.conversation),
                thread: self.status.thread.map(ThreadId),
            },
            id: MessageId(self.status.id),
        };
        (key, status)
    }
}

/// The statuses an earlier process left behind; none when nothing keeps statuses or they
/// cannot be read, which is reported.
pub(crate) async fn left_behind(shared: &Shared) -> Vec<(SessionKey, MessageRef)> {
    let Some(statuses) = shared.statuses() else {
        return Vec::new();
    };
    shared
        .bounded(None, "read the statuses an earlier process left", statuses.list())
        .await
        .unwrap_or_default()
}

/// Replace each status in `left` with [`Replies::interrupted`], then forget it.
///
/// Each is tried once, so a message the platform no longer lets the bot edit is not tried at
/// every start. The stop gives up on the rest, which stay kept.
pub(crate) async fn replace_left_behind(
    left: Vec<(SessionKey, MessageRef)>,
    channels: Vec<Arc<dyn ImChannel>>,
    shared: Arc<Shared>,
    stop: CancellationToken,
) {
    let text = Outbound::Text(shared.replies.interrupted.clone());
    for (key, status) in left {
        match channels.iter().find(|channel| channel.platform() == key.platform) {
            Some(channel) => {
                let act = format!("replace status {} that an earlier process left", status.id);
                tokio::select! {
                    biased;
                    () = stop.cancelled() => return,
                    _ = shared.bounded(Some(&key), &act, channel.update(&status, &text)) => {}
                }
            }
            None => shared.failed(
                Some(&key),
                format!(
                    "cannot replace status {} that an earlier process left: no channel is named {:?}",
                    status.id, key.platform
                ),
            ),
        }
        shared.forget_status(&key, &status).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(platform: &str, conversation: &str, thread: &str) -> SessionKey {
        SessionKey {
            platform: platform.into(),
            conversation: ConversationId(conversation.into()),
            thread: Some(ThreadId(thread.into())),
        }
    }

    fn status(key: &SessionKey, id: &str) -> MessageRef {
        MessageRef {
            at: key.destination(),
            id: MessageId(id.into()),
        }
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn a_kept_status_is_listed_until_it_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let store = StatusFiles::new(dir.path());
        let key = key("slack", "C1", "1700000000.000100");
        let posted = status(&key, "1700000000.000200");
        store.insert(&key, &posted).await.unwrap();
        assert_eq!(store.list().await.unwrap(), [(key.clone(), posted.clone())]);
        store.remove(&key, &posted).await.unwrap();
        assert!(store.list().await.unwrap().is_empty());
        // Removing it again changes nothing.
        store.remove(&key, &posted).await.unwrap();
        assert!(names(dir.path()).is_empty());
    }

    #[tokio::test]
    async fn a_record_is_one_json_file_named_after_its_message() {
        let dir = tempfile::tempdir().unwrap();
        let store = StatusFiles::new(dir.path());
        let key = key("slack", "C1", "1700000000.000100");
        let posted = MessageRef {
            at: Destination {
                conversation: ConversationId("C1".into()),
                thread: None,
            },
            id: MessageId("1700000000.000200".into()),
        };
        store.insert(&key, &posted).await.unwrap();
        let name = "slack.C1.1700000000%2E000200.json";
        assert_eq!(names(dir.path()), [name]);
        let record: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join(name)).unwrap()).unwrap();
        assert_eq!(
            record,
            serde_json::json!({
                "platform": "slack",
                "conversation": "C1",
                "thread": "1700000000.000100",
                "status": {"conversation": "C1", "thread": null, "id": "1700000000.000200"},
            })
        );
        assert_eq!(store.list().await.unwrap(), [(key, posted)]);
    }

    #[tokio::test]
    async fn ids_cannot_leave_the_directory_or_collide() {
        let dir = tempfile::tempdir().unwrap();
        let store = StatusFiles::new(dir.path());
        let odd = key("..", "a/b.c", "t");
        let plain = key("a", "b", "t");
        store.insert(&odd, &status(&odd, "../x")).await.unwrap();
        store.insert(&plain, &status(&plain, "c.x")).await.unwrap();
        assert_eq!(
            names(dir.path()),
            ["%2E%2E.a%2Fb%2Ec.%2E%2E%2Fx.json", "a.b.c%2Ex.json"]
        );
        assert_eq!(store.list().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_write_cut_short_is_passed_over_and_a_broken_record_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = StatusFiles::new(dir.path());
        std::fs::write(dir.path().join("slack.C1.m1.partial"), "{\"platform\":").unwrap();
        assert!(store.list().await.unwrap().is_empty());
        std::fs::write(dir.path().join("slack.C1.m2.json"), "{\"platform\":").unwrap();
        let error = store.list().await.unwrap_err();
        assert!(error.contains("slack.C1.m2.json is not a status record"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn only_the_owner_may_read_a_record() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let store = StatusFiles::new(dir.path());
        let key = key("slack", "C1", "t");
        store.insert(&key, &status(&key, "m")).await.unwrap();
        let mode = std::fs::metadata(dir.path().join("slack.C1.m.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[tokio::test]
    async fn a_missing_directory_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = StatusFiles::new(dir.path().join("missing"));
        let key = key("slack", "C1", "t");
        assert!(
            store
                .insert(&key, &status(&key, "m"))
                .await
                .unwrap_err()
                .contains("cannot write")
        );
        assert!(store.list().await.unwrap_err().contains("cannot read"));
    }
}
