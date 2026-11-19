//! The backlog of the program `zx-im` ([`BacklogFiles`]): a file per message pending, and
//! an empty one per message handled lately.

use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use zx_agent::ai::now_ms;
use zx_agent::builtin::files::create_private_dir;
use zx_util::sync::lock;

use crate::files::{RECORD, blocking, record_paths, remove_record, segment, write_record};
use crate::{ConversationId, MessageId};

use super::{Backlog, Kept, MessageKey};

/// How long a handled message stays known, so that a delivery of it again is not handled
/// twice: Slack retries a delayed event "once per hour for 24 hours" (Events API, "Delayed
/// events retry"), longer than its other retries and Feishu's.
const HANDLED_FOR: Duration = Duration::from_secs(24 * 60 * 60);
/// How often the marks of messages handled longer ago than that are removed, besides as the
/// server starts: a mark kept a little longer only keeps its message known longer.
const PRUNE_EVERY: Duration = Duration::from_secs(60 * 60);
/// The extension of a mark: an empty file saying that its message was handled.
const MARK: &str = "handled";
/// What [`blocking`] names in its error.
const FILES: &str = "backlog files";

/// A [`Backlog`] in a directory: `pending/` holds a JSON file per message pending, written
/// under a temporary name and then renamed, as status files are, and `handled/` an empty file
/// per message handled within the last 24 hours. Both are named
/// `<platform>.<conversation>.<message>`, each part encoded so that no ID can name another
/// file. On Unix only the owner may read them, as with session files.
///
/// Completing a message marks it handled first, then removes its record: a process that ends
/// between the two leaves both, which the next start takes as handled.
pub(crate) struct BacklogFiles {
    pending: PathBuf,
    handled: PathBuf,
    /// The order of the message kept last: the next one is later. [`pending`](Backlog::pending)
    /// raises it past the messages an earlier process kept.
    last: Mutex<u64>,
    /// When the marks were last pruned.
    pruned: Mutex<Instant>,
}

impl BacklogFiles {
    /// The backlog in `dir`, whose two directories are created when they are missing,
    /// readable only by this user. Fails when they cannot be.
    pub(crate) fn open(dir: &Path) -> io::Result<Self> {
        let pending = dir.join("pending");
        let handled = dir.join("handled");
        create_private_dir(&pending)?;
        create_private_dir(&handled)?;
        Ok(Self {
            pending,
            handled,
            last: Mutex::new(0),
            pruned: Mutex::new(Instant::now()),
        })
    }

    fn record_path(&self, key: &MessageKey) -> PathBuf {
        self.pending.join(format!("{}.{RECORD}", name(key)))
    }

    /// The order of a message kept now: the time in milliseconds, but later than the message
    /// kept last, should the clock not have moved or have gone back.
    fn next_order(&self) -> u64 {
        let mut last = lock(&self.last);
        *last = now_ms().max(last.saturating_add(1));
        *last
    }

    /// Whether the marks are due for pruning, which then counts as done now.
    fn prune_due(&self) -> bool {
        let mut pruned = lock(&self.pruned);
        if pruned.elapsed() < PRUNE_EVERY {
            return false;
        }
        *pruned = Instant::now();
        true
    }
}

/// `<platform>.<conversation>.<message>`, each part encoded ([`segment`]) so that it never
/// contains a `.`.
fn name(key: &MessageKey) -> String {
    format!(
        "{}.{}.{}",
        segment(&key.platform),
        segment(&key.conversation.0),
        segment(&key.message.0)
    )
}

fn mark_path(handled: &Path, key: &MessageKey) -> PathBuf {
    handled.join(format!("{}.{MARK}", name(key)))
}

#[async_trait]
impl Backlog for BacklogFiles {
    async fn keep(&self, key: &MessageKey, record: &str) -> Result<Kept, String> {
        let path = self.record_path(key);
        let mark = mark_path(&self.handled, key);
        let stored = Stored {
            platform: key.platform.clone(),
            conversation: key.conversation.0.clone(),
            message: key.message.0.clone(),
            order: self.next_order(),
            record: record.to_string(),
        };
        let text = serde_json::to_string(&stored)
            .map_err(|error| format!("cannot encode the record of {}: {error}", path.display()))?;
        blocking(FILES, move || {
            if exists(&path)? || exists(&mark)? {
                return Ok(Kept::Again);
            }
            write_record(&path, &text)?;
            Ok(Kept::New)
        })
        .await
    }

    async fn complete(&self, key: &MessageKey) -> Result<(), String> {
        let path = self.record_path(key);
        let handled = self.handled.clone();
        let mark = mark_path(&handled, key);
        let prune = self.prune_due();
        blocking(FILES, move || {
            write_mark(&mark)?;
            remove_record(&path)?;
            if prune {
                prune_marks(&handled);
            }
            Ok(())
        })
        .await
    }

    async fn pending(&self) -> Result<Vec<(MessageKey, String)>, String> {
        let (pending, handled) = (self.pending.clone(), self.handled.clone());
        let kept = blocking(FILES, move || {
            prune_marks(&handled);
            read_pending(&pending, &handled)
        })
        .await?;
        *lock(&self.pruned) = Instant::now();
        if let Some(latest) = kept.iter().map(|stored| stored.order).max() {
            let mut last = lock(&self.last);
            *last = (*last).max(latest);
        }
        Ok(kept.into_iter().map(Stored::into_parts).collect())
    }
}

/// Whether `path` exists; an error when that cannot be told.
fn exists(path: &Path) -> Result<bool, String> {
    path.try_exists()
        .map_err(|error| format!("cannot look for {}: {error}", path.display()))
}

/// Mark a message handled with an empty file at `path`.
fn write_mark(path: &Path) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
        .open(path)
        .map(drop)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))
}

/// Remove the marks in `handled` older than [`HANDLED_FOR`], by the time they were written.
/// A mark that cannot be read or removed now stays, and is pruned at a later time: it only
/// keeps its message known longer.
fn prune_marks(handled: &Path) {
    let Ok(entries) = std::fs::read_dir(handled) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let expired = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|written| now.duration_since(written).is_ok_and(|age| age > HANDLED_FOR));
        if expired {
            std::fs::remove_file(entry.path()).ok();
        }
    }
}

/// The records in `pending`, in the order kept. A record whose message is marked handled
/// in `handled` is one a process completed up to removing it: it is removed now.
fn read_pending(pending: &Path, handled: &Path) -> Result<Vec<Stored>, String> {
    let mut kept = Vec::new();
    for path in record_paths(pending)? {
        let text =
            std::fs::read_to_string(&path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        let stored: Stored = serde_json::from_str(&text)
            .map_err(|error| format!("{} is not a backlog record: {error}", path.display()))?;
        if exists(&mark_path(handled, &stored.key()))? {
            remove_record(&path)?;
            continue;
        }
        kept.push(stored);
    }
    kept.sort_by_key(|stored| stored.order);
    Ok(kept)
}

/// A message pending, as stored.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Stored {
    platform: String,
    conversation: String,
    message: String,
    /// Kept after every message with a lower order.
    order: u64,
    /// What the server keeps of the message.
    record: String,
}

impl Stored {
    fn key(&self) -> MessageKey {
        MessageKey {
            platform: self.platform.clone(),
            conversation: ConversationId(self.conversation.clone()),
            message: MessageId(self.message.clone()),
        }
    }

    fn into_parts(self) -> (MessageKey, String) {
        let key = self.key();
        (key, self.record)
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use super::*;

    fn key(message: &str) -> MessageKey {
        MessageKey {
            platform: "slack".into(),
            conversation: ConversationId("C1".into()),
            message: MessageId(message.into()),
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
    async fn what_is_pending_comes_back_after_a_restart_in_the_order_kept() {
        let dir = tempfile::tempdir().unwrap();
        let backlog = BacklogFiles::open(dir.path()).unwrap();
        for (message, record) in [("m2", "two"), ("m1", "one"), ("m3", "three")] {
            assert_eq!(backlog.keep(&key(message), record).await, Ok(Kept::New));
        }
        backlog.complete(&key("m1")).await.unwrap();

        let restarted = BacklogFiles::open(dir.path()).unwrap();
        let pending = restarted.pending().await.unwrap();
        assert_eq!(
            pending,
            [(key("m2"), "two".to_string()), (key("m3"), "three".to_string())]
        );
        // What is kept now comes after what an earlier process kept.
        restarted.keep(&key("m0"), "zero").await.unwrap();
        let order: Vec<MessageId> = restarted
            .pending()
            .await
            .unwrap()
            .into_iter()
            .map(|(key, _)| key.message)
            .collect();
        assert_eq!(
            order,
            [MessageId("m2".into()), MessageId("m3".into()), MessageId("m0".into())]
        );
    }

    #[tokio::test]
    async fn a_message_is_kept_once_and_known_for_a_day_after_it_is_handled() {
        let dir = tempfile::tempdir().unwrap();
        let backlog = BacklogFiles::open(dir.path()).unwrap();
        assert_eq!(backlog.keep(&key("1.5"), "a").await, Ok(Kept::New));
        assert_eq!(backlog.keep(&key("1.5"), "a").await, Ok(Kept::Again));
        backlog.complete(&key("1.5")).await.unwrap();
        assert!(names(&dir.path().join("pending")).is_empty());
        assert_eq!(names(&dir.path().join("handled")), ["slack.C1.1%2E5.handled"]);
        assert_eq!(backlog.keep(&key("1.5"), "a").await, Ok(Kept::Again));
        // Completing it again changes nothing.
        backlog.complete(&key("1.5")).await.unwrap();

        // A day later, the next start forgets it.
        let mark = File::options()
            .write(true)
            .open(dir.path().join("handled/slack.C1.1%2E5.handled"))
            .unwrap();
        mark.set_modified(SystemTime::now() - HANDLED_FOR - Duration::from_secs(60))
            .unwrap();
        let restarted = BacklogFiles::open(dir.path()).unwrap();
        assert!(restarted.pending().await.unwrap().is_empty());
        assert!(names(&dir.path().join("handled")).is_empty());
        assert_eq!(restarted.keep(&key("1.5"), "a").await, Ok(Kept::New));
    }

    #[tokio::test]
    async fn a_message_marked_handled_but_still_recorded_is_handled() {
        let dir = tempfile::tempdir().unwrap();
        let backlog = BacklogFiles::open(dir.path()).unwrap();
        backlog.keep(&key("m1"), "one").await.unwrap();
        // The process ended between marking the message and removing its record.
        write_mark(&mark_path(&dir.path().join("handled"), &key("m1"))).unwrap();

        let restarted = BacklogFiles::open(dir.path()).unwrap();
        assert!(restarted.pending().await.unwrap().is_empty());
        assert!(names(&dir.path().join("pending")).is_empty());
        assert_eq!(restarted.keep(&key("m1"), "one").await, Ok(Kept::Again));
    }

    #[tokio::test]
    async fn a_write_cut_short_is_passed_over_and_a_broken_record_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let backlog = BacklogFiles::open(dir.path()).unwrap();
        let pending = dir.path().join("pending");
        std::fs::write(pending.join("slack.C1.m1.partial"), "{\"platform\":").unwrap();
        assert!(backlog.pending().await.unwrap().is_empty());
        std::fs::write(pending.join("slack.C1.m2.json"), "{\"platform\":").unwrap();
        let error = backlog.pending().await.unwrap_err();
        assert!(error.contains("slack.C1.m2.json is not a backlog record"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn only_the_owner_may_read_a_record_or_a_mark() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let backlog = BacklogFiles::open(dir.path()).unwrap();
        backlog.keep(&key("m1"), "one").await.unwrap();
        backlog.keep(&key("m2"), "two").await.unwrap();
        backlog.complete(&key("m2")).await.unwrap();
        for path in ["pending/slack.C1.m1.json", "handled/slack.C1.m2.handled"] {
            let mode = std::fs::metadata(dir.path().join(path)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{path}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn only_the_owner_may_list_the_backlog() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        BacklogFiles::open(&dir.path().join(".backlog")).unwrap();
        for path in [".backlog", ".backlog/pending", ".backlog/handled"] {
            let mode = std::fs::metadata(dir.path().join(path)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{path}");
        }
    }

    #[tokio::test]
    async fn a_backlog_whose_directories_are_gone_fails_to_keep() {
        let dir = tempfile::tempdir().unwrap();
        let backlog = BacklogFiles::open(dir.path()).unwrap();
        std::fs::remove_dir(dir.path().join("pending")).unwrap();
        assert!(
            backlog
                .keep(&key("m1"), "one")
                .await
                .unwrap_err()
                .contains("cannot write")
        );
        assert!(backlog.pending().await.unwrap_err().contains("cannot read"));
    }
}
