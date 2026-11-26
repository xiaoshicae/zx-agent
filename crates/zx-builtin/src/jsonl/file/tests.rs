use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use zx_core::transcript::{SESSION_VERSION, Transcript};

use super::*;
use crate::ai::Message;
use crate::jsonl::test_support::{assistant, header, texts};

/// A new session in the file `path`.
fn create(path: &Path) -> Result<Transcript> {
    let header = header();
    let file = JsonlFile::create(path, &header)?;
    Ok(Transcript::with_log(header, Box::new(file)))
}

/// The session in the file `path`, loaded and open for appending.
fn open(path: &Path) -> Result<Transcript> {
    let (loaded, file) = JsonlFile::open(path)?;
    Transcript::load(loaded, Box::new(file))
}

/// A snapshot of the session in the file `path`.
fn snapshot(path: &Path) -> Result<Transcript> {
    Transcript::snapshot(read(path)?)
}

/// A session file with a header followed by `entries`, one per line, unterminated.
fn session_file(dir: &Path, entries: &[&str]) -> PathBuf {
    let path = dir.join("s.jsonl");
    let header = format!(r#"{{"type":"session","version":{SESSION_VERSION},"id":"h","timestamp":1,"cwd":"/"}}"#);
    let lines: Vec<&str> = std::iter::once(header.as_str())
        .chain(entries.iter().copied())
        .collect();
    std::fs::write(&path, lines.join("\n")).unwrap();
    path
}

fn user_entry(id: &str, parent: Option<&str>) -> String {
    let parent = parent.map_or("null".to_string(), |p| format!("\"{p}\""));
    format!(
        r#"{{"type":"message","id":"{id}","parentId":{parent},"timestamp":1,"message":{{"role":"user","content":"{id}","timestamp":1}}}}"#
    )
}

#[tokio::test]
async fn a_session_file_has_one_writer_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let transcript = create(&path).unwrap();

    let error = open(&path).err().unwrap().to_string();
    assert!(error.contains("is in use"), "{error}");

    drop(transcript);
    let mut reopened = open(&path).unwrap();
    reopened.append_message(Message::user("q")).await.unwrap();
    let error = open(&path).err().unwrap().to_string();
    assert!(error.contains("is in use"), "{error}");
}

#[tokio::test]
async fn a_snapshot_reads_a_session_in_use_without_changing_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let mut writer = create(&path).unwrap();
    writer.append_message(Message::user("q")).await.unwrap();
    // A record the writer has not finished yet.
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"type\":\"mess")
        .unwrap();
    let before = std::fs::read(&path).unwrap();

    let mut snapshot = snapshot(&path).unwrap();

    assert_eq!(texts(&snapshot), ["q"]);
    assert_eq!(snapshot.incomplete_tail_bytes(), "{\"type\":\"mess".len());
    let error = snapshot.append_message(Message::user("x")).await.unwrap_err();
    assert!(error.to_string().contains("read-only"), "{error}");
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(open(&path).is_err(), "the writer still holds the file");
}

#[cfg(unix)]
#[test]
fn a_dropped_session_file_can_be_reopened_while_this_process_starts_programs() {
    // A program being started holds copies of this process's descriptors until it runs,
    // so the lock of a transcript that was just dropped can outlive the transcript for a moment.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    drop(create(&path).unwrap());
    let starting = std::sync::atomic::AtomicBool::new(true);

    let failures = std::thread::scope(|scope| {
        scope.spawn(|| {
            while starting.load(std::sync::atomic::Ordering::Relaxed) {
                std::process::Command::new("true").status().unwrap();
            }
        });
        let failures: Vec<String> = (0..3000)
            .filter_map(|_| open(&path).err().map(|error| error.to_string()))
            .collect();
        starting.store(false, std::sync::atomic::Ordering::Relaxed);
        failures
    });

    assert!(
        failures.is_empty(),
        "{} reopenings failed: {:?}",
        failures.len(),
        failures.first()
    );
}

#[test]
fn a_newer_session_with_a_cut_short_tail_is_left_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("future.jsonl");
    std::fs::write(
        &path,
        "{\"type\":\"session\",\"version\":99,\"id\":\"h\",\"timestamp\":1,\"cwd\":\"/\"}\n{\"type\":\"mess",
    )
    .unwrap();
    let original = std::fs::read(&path).unwrap();

    assert!(open(&path).is_err());

    assert_eq!(std::fs::read(&path).unwrap(), original);
}

#[test]
fn damage_before_the_last_line_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = session_file(dir.path(), &["{broken", &user_entry("a", None)]);
    assert!(open(&path).is_err());
}

#[tokio::test]
async fn a_complete_last_line_without_newline_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let path = session_file(dir.path(), &[&user_entry("a", None)]);
    let mut transcript = open(&path).unwrap();
    transcript.append_message(Message::user("b")).await.unwrap();
    drop(transcript);
    assert_eq!(texts(&open(&path).unwrap()), ["a", "b"]);
}

#[tokio::test]
async fn recovers_a_tail_cut_inside_a_utf8_character() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("utf8.jsonl");
    let mut transcript = create(&path).unwrap();
    transcript.append_message(Message::user("keep")).await.unwrap();
    transcript.append_message(Message::user("中文")).await.unwrap();
    drop(transcript);
    let bytes = std::fs::read(&path).unwrap();
    let cut = bytes.windows("中".len()).position(|w| w == "中".as_bytes()).unwrap() + 2;
    OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(u64::try_from(cut).unwrap())
        .unwrap();

    let reopened = open(&path);

    assert!(reopened.is_ok(), "a partial UTF-8 tail was not repaired");
    assert_eq!(texts(&reopened.unwrap()), ["keep"]);
}

#[test]
fn does_not_delete_a_corrupt_newline_terminated_record() {
    let dir = tempfile::tempdir().unwrap();
    let path = session_file(dir.path(), &[&user_entry("a", None), "{broken}\n"]);
    let original = std::fs::read(&path).unwrap();

    let result = open(&path);

    assert!(result.is_err(), "a complete corrupt record was silently discarded");
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

#[test]
fn preserves_unknown_complete_records_even_without_a_newline() {
    let dir = tempfile::tempdir().unwrap();
    let path = session_file(dir.path(), &[&user_entry("a", None), r#"{"type":"future_kind"}"#]);
    let original = std::fs::read(&path).unwrap();

    assert!(open(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

#[tokio::test]
async fn an_incomplete_utf8_suffix_does_not_hide_an_earlier_syntax_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("invalid.jsonl");
    let mut transcript = create(&path).unwrap();
    transcript.append_message(Message::user("keep")).await.unwrap();
    drop(transcript);
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{broken}\xe4\xb8")
        .unwrap();
    let original = std::fs::read(&path).unwrap();

    assert!(
        open(&path).is_err(),
        "invalid JSON was mistaken for an interrupted UTF-8 write"
    );
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

#[tokio::test]
async fn opening_changes_nothing_and_the_first_append_cuts_off_an_incomplete_tail() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let mut transcript = create(&path).unwrap();
    transcript.append_message(Message::user("kept")).await.unwrap();
    drop(transcript);
    let complete = std::fs::read(&path).unwrap();
    let cut_short = b"{\"type\":\"message\",\"id\":\"cut-short";
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(cut_short)
        .unwrap();
    let crashed = std::fs::read(&path).unwrap();

    let mut reopened = open(&path).unwrap();

    assert_eq!(reopened.incomplete_tail_bytes(), cut_short.len());
    assert_eq!(std::fs::read(&path).unwrap(), crashed, "opening changed the file");
    reopened.append_message(Message::user("next")).await.unwrap();
    let written = std::fs::read(&path).unwrap();
    assert!(written.starts_with(&complete) && written.len() > complete.len());
    assert!(!written.windows(cut_short.len()).any(|bytes| bytes == cut_short));
    drop(reopened);
    assert_eq!(texts(&open(&path).unwrap()), ["kept", "next"]);
}

#[tokio::test]
async fn a_header_is_read_without_the_records() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let mut transcript = create(&path).unwrap();
    transcript.append_message(Message::user("q")).await.unwrap();

    // Also while the writer holds the file.
    assert_eq!(&read_header(&path).unwrap(), transcript.header());

    for (name, content, expected) in [
        ("empty.jsonl", "", "is empty"),
        ("blank.jsonl", "\n", "is empty"),
        ("other.jsonl", "{\"type\":\"message\"}\n", "invalid session header"),
    ] {
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        let error = read_header(&path).unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{name}: {error:#}");
    }
}

#[test]
fn blank_lines_before_the_header_are_skipped_by_every_reader() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let header = create(&path).unwrap().header().clone();
    let content = std::fs::read(&path).unwrap();
    std::fs::write(&path, [b"\n \n".as_slice(), &content].concat()).unwrap();

    assert_eq!(read_header(&path).unwrap(), header);
    assert_eq!(read(&path).unwrap().header, header);
}

#[cfg(unix)]
#[test]
fn new_session_files_are_private_to_the_owner() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("private.jsonl");
    create(&path).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[tokio::test]
async fn a_session_file_is_read_back_as_it_was_written() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let mut transcript = create(&path).unwrap();
    transcript.append_message(Message::user("q")).await.unwrap();
    transcript.append_message(assistant("a")).await.unwrap();
    let written = transcript.header().clone();
    drop(transcript);

    let reopened = open(&path).unwrap();

    assert_eq!(texts(&reopened), ["q", "a"]);
    assert_eq!(reopened.header(), &written);
}

#[test]
fn creating_never_overwrites_a_session_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    drop(create(&path).unwrap());

    assert!(create(&path).is_err());
}
