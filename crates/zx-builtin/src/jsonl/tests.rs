use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use super::test_support::{header, texts};
use super::*;
use crate::ai::Message;
use zx_core::transcript::{Entry, LogRecord, MessageEntry, SESSION_VERSION};

/// A header for a session started in `cwd` at `timestamp`, with `id`.
fn header_of(id: &str, cwd: &str, timestamp: u64) -> SessionHeader {
    SessionHeader {
        id: id.into(),
        timestamp,
        ..SessionHeader::new(Path::new(cwd), "m")
    }
}

/// Create the session of `header` in `storage` with one prompt, and release it. The prompt
/// goes to the log as a transcript writes it, but dated a millisecond after the session
/// started instead of now: a listing orders sessions by that time, and two prompts written
/// now can share a millisecond.
async fn seed(storage: &JsonlStorage, header: &SessionHeader, prompt: &str) {
    let entry = Entry::Message(MessageEntry {
        id: "e1".into(),
        parent_id: None,
        timestamp: header.timestamp + 1,
        message: Message::user(prompt),
        actor: None,
    });
    let json = serde_json::to_string(&entry).unwrap();
    let mut log = storage.create(header).await.unwrap();
    let record = LogRecord {
        seq: 1,
        id: entry.id(),
        parent_id: entry.parent_id(),
        kind: entry.kind(),
        timestamp: entry.timestamp(),
        json: &json,
    };
    log.append(&record).await.unwrap();
}

async fn ids(storage: &JsonlStorage, query: &SessionQuery) -> Vec<String> {
    let listed = storage.list(query).await.unwrap();
    listed.sessions.into_iter().map(|session| session.id).collect()
}

fn in_cwd(cwd: &str) -> SessionQuery {
    SessionQuery {
        cwd: Some(cwd.into()),
        ..SessionQuery::default()
    }
}

fn ids_of(headers: Vec<SessionHeader>) -> Vec<String> {
    headers.into_iter().map(|header| header.id).collect()
}

#[tokio::test]
async fn headers_name_the_sessions_a_query_asks_for_the_most_recently_started_first() {
    let dir = tempfile::tempdir().unwrap();
    let storage = JsonlStorage::in_directory(dir.path());
    for (id, started) in [("b", 1), ("a", 1), ("c", 2)] {
        seed(&storage, &header_of(id, "/w", started), id).await;
    }
    seed(&storage, &header_of("elsewhere", "/x", 3), "x").await;

    assert_eq!(ids_of(storage.headers(&in_cwd("/w")).await.unwrap()), ["c", "a", "b"]);
    let first = SessionQuery {
        limit: Some(1),
        ..in_cwd("/w")
    };
    assert_eq!(ids_of(storage.headers(&first).await.unwrap()), ["c"]);
    assert_eq!(
        ids_of(storage.headers(&SessionQuery::default()).await.unwrap()),
        ["elsewhere", "c", "a", "b"]
    );
    std::fs::remove_file(storage.path_of("c").await.unwrap().unwrap()).unwrap();
    assert_eq!(ids_of(storage.headers(&in_cwd("/w")).await.unwrap()), ["a", "b"]);
}

#[tokio::test]
async fn headers_read_no_records_and_leave_out_what_cannot_be_opened() {
    let dir = tempfile::tempdir().unwrap();
    let storage = JsonlStorage::in_directory(dir.path());
    seed(&storage, &header_of("a", "/w", 1), "q").await;
    let path = storage.path_of("a").await.unwrap().unwrap();
    let bytes = std::fs::read_to_string(&path).unwrap();
    let (header, last) = bytes.split_once('\n').unwrap();
    std::fs::write(&path, format!("{header}\ninvalid interior record\n{last}")).unwrap();
    std::fs::write(path.with_file_name("garbage.jsonl"), "not a header\n").unwrap();
    let older = header
        .replace(&format!("\"version\":{SESSION_VERSION}"), "\"version\":1")
        .replace("\"id\":\"a\"", "\"id\":\"older\"");
    std::fs::write(path.with_file_name("older.jsonl"), older + "\n").unwrap();

    let headers = storage.headers(&in_cwd("/w")).await.unwrap();

    assert_eq!(
        ids_of(headers),
        ["a"],
        "damage past the header shows when the session is opened"
    );
    let list = storage.list(&in_cwd("/w")).await.unwrap();
    assert!(list.sessions.is_empty());
    assert_eq!(list.unreadable.len(), 3, "{:?}", list.unreadable);
}

#[test]
fn a_time_is_written_as_utc_without_colons() {
    for (ms, expected) in [
        (0, "1970-01-01T00-00-00-000Z"),
        (946_684_799_999, "1999-12-31T23-59-59-999Z"),
        (951_782_400_000, "2000-02-29T00-00-00-000Z"),
        (1_709_251_199_000, "2024-02-29T23-59-59-000Z"),
        (1_709_251_200_000, "2024-03-01T00-00-00-000Z"),
        (1_762_300_000_123, "2025-11-04T23-46-40-123Z"),
        (4_102_444_800_000, "2100-01-01T00-00-00-000Z"),
    ] {
        assert_eq!(utc_time(ms), expected, "{ms}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_directory_that_cannot_be_read_is_reported_and_keeps_no_other_session_from_its_storage() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let storage = JsonlStorage::in_directory(dir.path());
    seed(&storage, &header_of("kept", "/w", 1), "kept").await;
    // The directory of another working directory, which this user cannot read.
    let locked = storage.location(Path::new("/locked"));
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read_dir(&locked).is_ok() {
        // Root reads every directory, so there is nothing to show.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        return;
    }

    let created = storage.create(&header_of("new", "/w", 2)).await.map(drop);
    let opened = storage.open("kept").await.map(drop);
    let listed = storage.list(&SessionQuery::default()).await;
    let in_locked = storage.list(&in_cwd("/locked")).await;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();

    assert!(created.is_ok(), "{created:?}");
    assert!(opened.is_ok(), "{opened:?}");
    for listed in [listed.unwrap(), in_locked.unwrap()] {
        assert_eq!(listed.unreadable.len(), 1, "{:?}", listed.unreadable);
        assert_eq!(listed.unreadable[0].location, locked.display().to_string());
    }
}

#[tokio::test]
async fn a_session_is_kept_in_the_directory_of_its_working_directory() {
    let dir = tempfile::tempdir().unwrap();
    let storage = JsonlStorage::in_directory(dir.path());
    let header = header_of("0b7c", "/Users/me/work:repo", 1_762_300_000_123);

    seed(&storage, &header, "q").await;

    let directory = dir.path().join("--Users-me-work-repo--");
    let path = directory.join("2025-11-04T23-46-40-123Z_0b7c.jsonl");
    assert!(
        path.is_file(),
        "{:?}",
        std::fs::read_dir(dir.path()).unwrap().collect::<Vec<_>>()
    );
    assert_eq!(storage.location(Path::new("/Users/me/work:repo")), directory);
    assert_eq!(storage.path_of("0b7c").await.unwrap(), Some(path));
    assert_eq!(storage.path_of("other").await.unwrap(), None);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&directory).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}

#[tokio::test]
async fn a_session_is_opened_by_its_id_whatever_its_file_is_called() {
    let dir = tempfile::tempdir().unwrap();
    let directory = dir.path().join("--w--");
    std::fs::create_dir_all(&directory).unwrap();
    let line = |id: &str| {
        format!(r#"{{"type":"session","version":{SESSION_VERSION},"id":"{id}","timestamp":1,"cwd":"/w","model":"m"}}"#)
    };
    std::fs::write(directory.join("renamed.jsonl"), format!("{}\n", line("abc"))).unwrap();
    std::fs::write(directory.join("abd.jsonl"), format!("{}\n", line("abd"))).unwrap();
    std::fs::write(directory.join("broken.jsonl"), "not json\n").unwrap();
    // A fresh storage each time: nothing is known before the session is asked for.
    for id in ["abc", "abd"] {
        let storage = JsonlStorage::in_directory(dir.path());

        let (loaded, _log) = storage.open(id).await.unwrap();

        assert_eq!(loaded.header.id, id);
    }

    let storage = JsonlStorage::in_directory(dir.path());
    for error in [
        storage.open("ab").await.err().unwrap(),
        storage.read("ab").await.err().unwrap(),
    ] {
        let expected = format!("no session ab in {}", dir.path().display());
        assert_eq!(error.to_string(), expected);
    }
}

#[tokio::test]
async fn a_listing_asks_by_working_directory_and_label_and_reports_what_it_cannot_read() {
    let dir = tempfile::tempdir().unwrap();
    let storage = JsonlStorage::in_directory(dir.path());
    let chat = |value: &str| BTreeMap::from([("chat".to_string(), value.to_string())]);
    let first = SessionHeader {
        labels: chat("a"),
        ..header_of("first", "/w", 1)
    };
    let second = SessionHeader {
        labels: chat("a"),
        ..header_of("second", "/w", 2)
    };
    let other = SessionHeader {
        labels: chat("b"),
        ..header_of("other", "/w", 3)
    };
    for header in [&first, &second, &other] {
        seed(&storage, header, &header.id).await;
    }
    seed(&storage, &header_of("elsewhere", "/x", 4), "elsewhere").await;
    let broken = storage.location(Path::new("/w")).join("broken.jsonl");
    std::fs::write(&broken, "not json\n").unwrap();

    assert_eq!(ids(&storage, &in_cwd("/w")).await.len(), 3);
    assert_eq!(ids(&storage, &in_cwd("/nowhere")).await, [""; 0]);
    assert_eq!(ids(&storage, &SessionQuery::default()).await.len(), 4);
    let latest_of_chat = SessionQuery {
        labels: chat("a"),
        limit: Some(1),
        ..SessionQuery::default()
    };
    let listed = storage.list(&latest_of_chat).await.unwrap();
    assert_eq!(listed.sessions.len(), 1);
    let session = &listed.sessions[0];
    assert_eq!(
        (session.id.as_str(), session.first_prompt.as_deref()),
        ("second", Some("second"))
    );
    assert_eq!(session.labels, chat("a"));
    assert_eq!(listed.unreadable.len(), 1, "{:?}", listed.unreadable);
    assert_eq!(listed.unreadable[0].location, broken.display().to_string());
    assert!(listed.unreadable[0].error.contains("invalid session header"));
}

#[tokio::test]
async fn a_listing_shows_what_a_session_recorded_since_it_was_last_listed() {
    let dir = tempfile::tempdir().unwrap();
    let storage = JsonlStorage::in_directory(dir.path());
    let header = header_of("grows", "/w", 1);
    let mut session = Transcript::with_log(header.clone(), storage.create(&header).await.unwrap());
    session.append_message(Message::user("first")).await.unwrap();
    let listed = storage.list(&in_cwd("/w")).await.unwrap().sessions;
    assert_eq!(
        (listed[0].messages, listed[0].first_prompt.as_deref()),
        (1, Some("first"))
    );
    // Listed again unchanged, it reads the same.
    assert_eq!(storage.list(&in_cwd("/w")).await.unwrap().sessions, listed);

    session.append_message(Message::user("second")).await.unwrap();

    let again = storage.list(&in_cwd("/w")).await.unwrap().sessions;
    assert_eq!(again[0].messages, 2);
    assert!(again[0].last_activity_ms >= listed[0].last_activity_ms);
}

#[tokio::test]
async fn sessions_another_storage_created_are_found_and_deleted_ones_are_not_listed() {
    let dir = tempfile::tempdir().unwrap();
    let storage = JsonlStorage::in_directory(dir.path());
    seed(&storage, &header_of("mine", "/w", 1), "q").await;
    assert_eq!(ids(&storage, &in_cwd("/w")).await, ["mine"]);

    // Another process, as far as this storage knows.
    seed(
        &JsonlStorage::in_directory(dir.path()),
        &header_of("theirs", "/w", 2),
        "q",
    )
    .await;
    assert_eq!(ids(&storage, &in_cwd("/w")).await, ["theirs", "mine"]);
    assert!(storage.read("theirs").await.is_ok());

    std::fs::remove_file(storage.path_of("mine").await.unwrap().unwrap()).unwrap();
    assert_eq!(ids(&storage, &in_cwd("/w")).await, ["theirs"]);
    assert_eq!(storage.path_of("mine").await.unwrap(), None);
}

#[tokio::test]
async fn a_session_id_names_one_session_of_the_storage() {
    let dir = tempfile::tempdir().unwrap();
    let storage = JsonlStorage::in_directory(dir.path());
    seed(&storage, &header_of("same", "/w", 1), "q").await;

    // Another working directory and another time, so another file name.
    let error = storage.create(&header_of("same", "/x", 2)).await.err().unwrap();

    assert!(error.to_string().contains("session same exists already"), "{error}");
    assert_eq!(ids(&storage, &SessionQuery::default()).await, ["same"]);
}

#[tokio::test]
async fn an_id_that_could_name_another_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let storage = JsonlStorage::in_directory(dir.path());
    for id in ["", "../escape", "a/b", "a b", "名"] {
        let error = storage.create(&header_of(id, "/w", 1)).await.err().unwrap();

        assert!(
            error.to_string().contains("cannot name a session file"),
            "{id:?}: {error}"
        );
    }
    assert!(
        std::fs::read_dir(dir.path()).unwrap().next().is_none(),
        "nothing was created"
    );
}

#[tokio::test]
async fn one_file_holds_at_most_one_session() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested").join("run.jsonl");
    let storage = JsonlStorage::file(&path);
    assert!(ids(&storage, &SessionQuery::default()).await.is_empty());
    assert_eq!(storage.path_of("any").await.unwrap(), None);
    let header = header();

    seed(&storage, &header, "q").await;

    assert_eq!(storage.location(Path::new("/anywhere")), path);
    assert_eq!(ids(&storage, &SessionQuery::default()).await, [header.id.as_str()]);
    assert_eq!(storage.path_of(&header.id).await.unwrap(), Some(path.clone()));
    let second = storage.create(&super::test_support::header()).await;
    assert!(second.err().unwrap().to_string().contains("cannot create session file"));
    let error = storage.open("other").await.err().unwrap();
    assert_eq!(error.to_string(), format!("no session other in {}", path.display()));
    let (loaded, log) = storage.open(&header.id).await.unwrap();
    assert_eq!(texts(&Transcript::load(loaded, log).unwrap()), ["q"]);
}

#[tokio::test]
async fn a_damaged_file_is_an_error_when_it_is_the_only_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("run.jsonl");
    std::fs::write(&path, "not json\n").unwrap();
    let storage = JsonlStorage::file(&path);

    let error = storage.open("any").await.err().unwrap();

    assert!(format!("{error:#}").contains("invalid session header"), "{error:#}");
    let listed = storage.list(&SessionQuery::default()).await.unwrap();
    assert!(listed.sessions.is_empty());
    assert_eq!(listed.unreadable.len(), 1);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json\n");
}

/// A session in the file `path`, made by the storage of that one file.
async fn create_file(path: &Path) -> Transcript {
    let header = header();
    let log = JsonlStorage::file(path).create(&header).await.unwrap();
    Transcript::with_log(header, log)
}

/// The session in the file `path`, opened by the storage of that one file.
async fn open_file(path: &Path) -> Transcript {
    let storage = JsonlStorage::file(path);
    let id = file::read_header(path).unwrap().id;
    let (loaded, log) = storage.open(&id).await.unwrap();
    Transcript::load(loaded, log).unwrap()
}

#[tokio::test]
async fn a_file_write_that_blocks_times_out_without_stalling_the_runtime() {
    /// Answers every write late, like a session volume that stopped answering; bounded,
    /// so that a transcript that waits for it still ends.
    struct Stuck;
    impl Write for Stuck {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            std::thread::sleep(std::time::Duration::from_secs(2));
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let log = JsonlFile::with_writer(Path::new("s.jsonl"), Box::new(Stuck));
    let mut transcript = Transcript::with_log(header(), Box::new(log));
    let started = std::time::Instant::now();

    let error = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        transcript.append_message(Message::user("q")),
    )
    .await;

    assert!(error.is_err(), "the append waited for the stuck write");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_failed_write_names_the_session_file() {
    /// Refuses every write, like a full disk.
    struct Full;
    impl Write for Full {
        fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk full"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let log = JsonlFile::with_writer(Path::new("s.jsonl"), Box::new(Full));
    let mut transcript = Transcript::with_log(header(), Box::new(log));

    let error = transcript.append_message(Message::user("q")).await.unwrap_err();

    assert_eq!(format!("{error:#}"), "cannot write session file s.jsonl: disk full");
}

/// Keeps `keep` bytes of the first write and then fails once, like a disk that fills up
/// in the middle of a record and is freed again. Later writes succeed.
struct PartialWriteOnce {
    file: std::fs::File,
    keep: usize,
    calls: usize,
}

impl Write for PartialWriteOnce {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.calls += 1;
        match self.calls {
            1 => {
                let part = &bytes[..self.keep.min(bytes.len())];
                self.file.write_all(part)?;
                Ok(part.len())
            }
            2 => Err(std::io::Error::other("disk full")),
            _ => self.file.write(bytes),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

#[tokio::test]
async fn a_partial_write_stops_later_writes_and_leaves_a_repairable_tail() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let mut transcript = create_file(&path).await;
    transcript.append_message(Message::user("kept")).await.unwrap();
    drop(transcript);
    let file = OpenOptions::new().append(true).open(&path).unwrap();
    let writer = PartialWriteOnce {
        file,
        keep: 10,
        calls: 0,
    };
    let mut transcript = Transcript::with_log(header(), Box::new(JsonlFile::with_writer(&path, Box::new(writer))));

    assert!(transcript.append_message(Message::user("cut short")).await.is_err());
    // A run keeps emitting messages until it observes its cancellation.
    let later = transcript.append_message(Message::user("after the failure")).await;
    assert!(later.is_err(), "an entry was written after a failed write");
    drop(transcript);

    let reopened = open_file(&path).await;
    assert_eq!(reopened.incomplete_tail_bytes(), 10);
    assert_eq!(texts(&reopened), ["kept"]);
}

#[tokio::test]
async fn files_of_an_older_or_newer_version_are_refused_and_left_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    for version in [1, SESSION_VERSION - 1, SESSION_VERSION + 1] {
        let path = dir.path().join(format!("v{version}.jsonl"));
        let header =
            serde_json::json!({"type":"session","version":version,"id":"h","timestamp":1,"cwd":"/","model":"m"});
        let message =
            serde_json::json!({"type":"message","id":"a","parentId":null,"timestamp":2,"message":Message::user("hi")});
        let content = format!("{header}\n{message}\n");
        std::fs::write(&path, &content).unwrap();
        let storage = JsonlStorage::file(&path);

        let (loaded, log) = storage.open("h").await.unwrap();
        assert!(Transcript::load(loaded, log).is_err());
        assert!(Transcript::snapshot(storage.read("h").await.unwrap()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
    }
}

#[tokio::test]
async fn a_file_of_another_version_is_not_created() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let older = SessionHeader {
        version: SESSION_VERSION - 1,
        ..header()
    };

    assert!(JsonlStorage::file(&path).create(&older).await.is_err());
    assert!(!path.exists());
}
