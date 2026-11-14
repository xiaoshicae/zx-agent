use std::collections::BTreeMap;
use std::path::Path;

use super::*;
use crate::ai::Message;
use crate::transcript::test_support::{header, texts};
use crate::transcript::{Entry, LogRecord, MessageEntry, SESSION_VERSION};

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
    let second = storage.create(&crate::transcript::test_support::header()).await;
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
