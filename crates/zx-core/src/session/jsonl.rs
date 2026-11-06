//! The JSONL session file: a header line, then one record per line.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
#[cfg(unix)]
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, ensure};
use async_trait::async_trait;
use serde::de::IgnoredAny;
use tokio::sync::oneshot;

use super::log::{LoadedSession, LogRecord, SessionLog};
use super::{HeaderLine, SessionHeader};

/// A session file open for appending. On Unix it holds an exclusive `flock(2)` lock until it
/// is dropped, so a second writer, in this process or another, cannot open the same file.
pub(super) struct JsonlFile {
    writer: Writer,
    /// The `seq` the next record must have.
    next_seq: u64,
}

/// Records reach the file on a thread of their own. A write to a volume that stopped
/// answering blocks inside the kernel; on that thread it stalls neither the runtime nor the
/// store's append timeout, and it does not keep the process from exiting.
struct Writer {
    lines: Option<mpsc::SyncSender<Line>>,
    thread: Option<thread::JoinHandle<()>>,
    /// Whether a line was sent and its result has not been received.
    pending: bool,
}

/// One record's line and where the result of writing it goes.
struct Line {
    text: String,
    written: oneshot::Sender<std::io::Result<()>>,
}

impl Writer {
    fn spawn(mut target: Box<dyn Write + Send>) -> Result<Self> {
        // One line at a time: the store waits for each append before the next one.
        let (lines, received) = mpsc::sync_channel::<Line>(1);
        let thread = thread::Builder::new()
            .name("zx-session-writer".into())
            .spawn(move || {
                for line in received {
                    let result = target.write_all(line.text.as_bytes()).and_then(|()| target.flush());
                    // The store may have stopped waiting for this write; then no one needs
                    // its result.
                    let _ = line.written.send(result);
                }
            })
            .context("cannot start the session writer thread")?;
        Ok(Self {
            lines: Some(lines),
            thread: Some(thread),
            pending: false,
        })
    }

    async fn write(&mut self, text: String) -> Result<()> {
        let lines = self
            .lines
            .as_ref()
            .ok_or_else(|| anyhow!("the session writer has stopped"))?;
        let (written, result) = oneshot::channel();
        lines
            .try_send(Line { text, written })
            .map_err(|_| anyhow!("an earlier write of the session file has not finished"))?;
        self.pending = true;
        let result = result
            .await
            .map_err(|_| anyhow!("the session writer stopped before the write finished"))?;
        self.pending = false;
        Ok(result?)
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // Without a sender, the thread ends once it has written what it was sent.
        drop(self.lines.take());
        // An idle thread ends at once, closing the file and releasing its lock before this
        // returns. One stuck in a write is left to finish on its own.
        if !self.pending
            && let Some(thread) = self.thread.take()
        {
            // A panic on that thread already failed its write; nothing is left to report.
            let _ = thread.join();
        }
    }
}

/// A locked session file that has been read but not yet changed. Its records are checked
/// first; only [`Unrepaired::repair`] changes the file and allows appending.
pub(super) struct Unrepaired {
    path: PathBuf,
    file: File,
    repair: Repair,
    next_seq: u64,
}

/// What the file needs before the first append.
enum Repair {
    /// The file ends with a complete record and its newline.
    Nothing,
    /// Cut off the incomplete final record, which starts at this offset.
    Truncate(u64),
    /// The last complete record lacks its newline; add it.
    Terminate,
}

impl JsonlFile {
    /// Create a new file holding only `header`. Fails if the file exists. Unlike pi, the
    /// header is written immediately, so even a run that crashes before the first answer
    /// leaves a record.
    pub(super) fn create(path: &Path, header: &SessionHeader) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let mut options = OpenOptions::new();
        options.append(true).create_new(true);
        // Tool output in the session can contain secrets, so only the owner may read the file.
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options
            .open(path)
            .with_context(|| format!("cannot create session file {}", path.display()))?;
        lock(&file, path)?;
        let line = serde_json::to_string(&HeaderLine::Session(header.clone()))?;
        writeln!(file, "{line}")
            .and_then(|()| file.flush())
            .with_context(|| format!("cannot write the header of {}", path.display()))?;
        Ok(Self {
            writer: Writer::spawn(Box::new(file))?,
            next_seq: 1,
        })
    }

    /// Lock an existing file and read its header and records without changing it.
    ///
    /// A crash during the final write can leave an incomplete last line. It is left out of the
    /// records and removed by [`Unrepaired::repair`]; damage anywhere else is an error.
    pub(super) fn open(path: &Path) -> Result<(LoadedSession, Unrepaired)> {
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(path)
            .with_context(|| format!("cannot open session file {}", path.display()))?;
        // Before reading: a concurrent writer's unfinished line must not look like a crash.
        lock(&file, path)?;
        let mut content = Vec::new();
        file.read_to_end(&mut content)
            .with_context(|| format!("cannot read session file {}", path.display()))?;
        let (loaded, truncate_at) = parse(&content, path)?;
        let repair = match truncate_at {
            Some(at) => Repair::Truncate(u64::try_from(at)?),
            None if !content.is_empty() && !content.ends_with(b"\n") => Repair::Terminate,
            None => Repair::Nothing,
        };
        let unrepaired = Unrepaired {
            path: path.to_path_buf(),
            file,
            repair,
            next_seq: u64::try_from(loaded.records.len())? + 1,
        };
        Ok((loaded, unrepaired))
    }

    /// Records go to `writer`, so tests can make writes fail. No header is written.
    #[cfg(test)]
    pub(super) fn with_writer(writer: Box<dyn Write + Send + Sync>) -> Self {
        Self {
            writer: Writer::spawn(writer).expect("tests can start a thread"),
            next_seq: 1,
        }
    }
}

/// Read a session file without locking or changing it. An incomplete last line, such as a
/// record that a writer has not finished, is left out.
pub(super) fn read(path: &Path) -> Result<LoadedSession> {
    let content = std::fs::read(path).with_context(|| format!("cannot read session file {}", path.display()))?;
    Ok(parse(&content, path)?.0)
}

/// The header and complete records of `content`, and where an incomplete last line starts.
fn parse(content: &[u8], path: &Path) -> Result<(LoadedSession, Option<usize>)> {
    // A short final write can end inside a UTF-8 character, so check one record at a
    // time rather than rejecting the whole file during UTF-8 decoding.
    let mut lines = Vec::new();
    let mut offset = 0;
    for raw in content.split_inclusive(|&byte| byte == b'\n') {
        if !raw.iter().all(u8::is_ascii_whitespace) {
            lines.push((offset, raw));
        }
        offset += raw.len();
    }
    let (_, first) = lines
        .first()
        .ok_or_else(|| anyhow!("session file {} is empty", path.display()))?;
    let HeaderLine::Session(header) =
        serde_json::from_slice(first).with_context(|| format!("{}: invalid session header", path.display()))?;

    let mut records = Vec::new();
    let mut incomplete = None;
    for (position, &(line_offset, raw)) in lines.iter().enumerate().skip(1) {
        if position + 1 == lines.len() && incomplete_tail(raw) {
            incomplete = Some(line_offset);
            break;
        }
        let record =
            std::str::from_utf8(raw).map_err(|error| anyhow!("{}: entry {position}: {error}", path.display()))?;
        records.push(record.trim_end().to_string());
    }
    let loaded = LoadedSession {
        header,
        records,
        repaired_bytes: incomplete.map_or(0, |start| content.len() - start),
    };
    Ok((loaded, incomplete))
}

impl Unrepaired {
    /// Cut off an incomplete final record, or terminate a complete one that lacks its
    /// newline, so that the next record starts on a line of its own.
    pub(super) fn repair(self) -> Result<JsonlFile> {
        let Self {
            path,
            file,
            repair,
            next_seq,
        } = self;
        match repair {
            Repair::Truncate(length) => file
                .set_len(length)
                .with_context(|| format!("cannot repair session file {}", path.display()))?,
            Repair::Terminate => (&file)
                .write_all(b"\n")
                .with_context(|| format!("cannot repair session file {}", path.display()))?,
            Repair::Nothing => {}
        }
        Ok(JsonlFile {
            writer: Writer::spawn(Box::new(file))?,
            next_seq,
        })
    }
}

#[async_trait]
impl SessionLog for JsonlFile {
    async fn append(&mut self, record: &LogRecord<'_>) -> Result<()> {
        ensure!(
            record.seq == self.next_seq,
            "record {} is out of order: the session file expects record {}",
            record.seq,
            self.next_seq
        );
        // The line survives a crash of this process, not of the machine: there is no fsync.
        let mut line = String::with_capacity(record.json.len() + 1);
        line.push_str(record.json);
        line.push('\n');
        self.writer.write(line).await?;
        self.next_seq += 1;
        Ok(())
    }
}

/// How long opening keeps trying a lock that another descriptor holds before reporting the file
/// as in use. A program that this process is starting holds copies of its descriptors until it
/// runs, so a store that was just dropped can keep its lock for a moment: under a millisecond
/// when measured on macOS, longer on a busy machine.
#[cfg(unix)]
const LOCK_WAIT: Duration = Duration::from_millis(500);

/// Longest pause between two attempts; the first pause is 1 ms and each one doubles.
#[cfg(unix)]
const LOCK_RETRY_MAX: Duration = Duration::from_millis(50);

/// Take an exclusive lock on the whole file. It lasts until the file is closed, including when
/// the process exits or is killed. A lock held elsewhere is retried for up to [`LOCK_WAIT`].
#[cfg(unix)]
fn lock(file: &File, path: &Path) -> Result<()> {
    use std::os::fd::AsRawFd;

    let started = Instant::now();
    let mut pause = Duration::from_millis(1);
    loop {
        // SAFETY: flock(2) has no memory-safety preconditions; the descriptor stays open for
        // the call because `file` is borrowed.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        match error.kind() {
            // A signal interrupted the call: try again at once.
            std::io::ErrorKind::Interrupted => {}
            std::io::ErrorKind::WouldBlock if started.elapsed() < LOCK_WAIT => {
                // A synchronous pause: opening happens once per session, and it waits only
                // while another descriptor holds the lock.
                std::thread::sleep(pause);
                pause = (pause * 2).min(LOCK_RETRY_MAX);
            }
            std::io::ErrorKind::WouldBlock => anyhow::bail!(
                "session file {} is in use: another process or session has it open for writing",
                path.display()
            ),
            _ => return Err(error).with_context(|| format!("cannot lock session file {}", path.display())),
        }
    }
}

/// Without `flock(2)`, two writers are not detected.
#[cfg(not(unix))]
fn lock(_file: &File, _path: &Path) -> Result<()> {
    Ok(())
}

/// Whether `raw`, the last line, is a record that a crash cut short: it lacks its newline and
/// is a valid but unfinished JSON text. A complete record, even one that is not a known
/// entry, is never treated as incomplete, so it is reported instead of silently removed.
fn incomplete_tail(raw: &[u8]) -> bool {
    if raw.ends_with(b"\n") {
        return false;
    }
    match serde_json::from_slice::<IgnoredAny>(raw) {
        Ok(_) => false,
        Err(error) if error.is_eof() => true,
        // Only a valid, unfinished JSON prefix may be repaired. An incomplete UTF-8 suffix
        // must not hide an unrelated syntax error earlier in the record.
        Err(_) => match std::str::from_utf8(raw) {
            Err(utf8) if utf8.error_len().is_none() => {
                serde_json::from_slice::<IgnoredAny>(&raw[..utf8.valid_up_to()]).is_err_and(|error| error.is_eof())
            }
            Ok(_) | Err(_) => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::path::{Path, PathBuf};

    use crate::ai::Message;
    use crate::session::SessionStore;
    use crate::session::test_support::{assistant, header, texts};

    /// A session file with a v2 header followed by `entries`, one per line, unterminated.
    fn session_file(dir: &Path, entries: &[&str]) -> PathBuf {
        let path = dir.join("s.jsonl");
        let header = r#"{"type":"session","version":2,"id":"h","timestamp":1,"cwd":"/","model":"m"}"#;
        let lines: Vec<&str> = std::iter::once(header).chain(entries.iter().copied()).collect();
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
        let store = SessionStore::create(&path, header()).unwrap();

        let error = SessionStore::open(&path).err().unwrap().to_string();
        assert!(error.contains("is in use"), "{error}");

        drop(store);
        let mut reopened = SessionStore::open(&path).unwrap();
        reopened.append_message(Message::user("q")).await.unwrap();
        let error = SessionStore::open(&path).err().unwrap().to_string();
        assert!(error.contains("is in use"), "{error}");
    }

    #[tokio::test]
    async fn a_snapshot_reads_a_session_in_use_without_changing_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut writer = SessionStore::create(&path, header()).unwrap();
        writer.append_message(Message::user("q")).await.unwrap();
        // A record the writer has not finished yet.
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"type\":\"mess")
            .unwrap();
        let before = std::fs::read(&path).unwrap();

        let mut snapshot = SessionStore::read(&path).unwrap();

        assert_eq!(texts(&snapshot), ["q"]);
        assert_eq!(snapshot.recovered_tail_bytes(), "{\"type\":\"mess".len());
        let error = snapshot.append_message(Message::user("x")).await.unwrap_err();
        assert!(error.to_string().contains("read-only"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(SessionStore::open(&path).is_err(), "the writer still holds the file");
    }

    #[cfg(unix)]
    #[test]
    fn a_dropped_store_can_be_reopened_while_this_process_starts_programs() {
        // A program being started holds copies of this process's descriptors until it runs,
        // so the lock of a store that was just dropped can outlive the store for a moment.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        drop(SessionStore::create(&path, header()).unwrap());
        let starting = std::sync::atomic::AtomicBool::new(true);

        let failures = std::thread::scope(|scope| {
            scope.spawn(|| {
                while starting.load(std::sync::atomic::Ordering::Relaxed) {
                    std::process::Command::new("true").status().unwrap();
                }
            });
            let failures: Vec<String> = (0..3000)
                .filter_map(|_| SessionStore::open(&path).err().map(|error| error.to_string()))
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
            "{\"type\":\"session\",\"version\":99,\"id\":\"h\",\"timestamp\":1,\"cwd\":\"/\",\"model\":\"m\"}\n{\"type\":\"mess",
        )
        .unwrap();
        let original = std::fs::read(&path).unwrap();

        assert!(SessionStore::open(&path).is_err());

        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn damage_before_the_last_line_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = session_file(dir.path(), &["{broken", &user_entry("a", None)]);
        assert!(SessionStore::open(&path).is_err());
    }

    #[tokio::test]
    async fn a_complete_last_line_without_newline_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = session_file(dir.path(), &[&user_entry("a", None)]);
        let mut store = SessionStore::open(&path).unwrap();
        store.append_message(Message::user("b")).await.unwrap();
        drop(store);
        assert_eq!(texts(&SessionStore::open(&path).unwrap()), ["a", "b"]);
    }

    #[tokio::test]
    async fn recovers_a_tail_cut_inside_a_utf8_character() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utf8.jsonl");
        let mut store = SessionStore::create(&path, header()).unwrap();
        store.append_message(Message::user("keep")).await.unwrap();
        store.append_message(Message::user("中文")).await.unwrap();
        drop(store);
        let bytes = std::fs::read(&path).unwrap();
        let cut = bytes.windows("中".len()).position(|w| w == "中".as_bytes()).unwrap() + 2;
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(u64::try_from(cut).unwrap())
            .unwrap();

        let reopened = SessionStore::open(&path);

        assert!(reopened.is_ok(), "a partial UTF-8 tail was not repaired");
        assert_eq!(texts(&reopened.unwrap()), ["keep"]);
    }

    #[test]
    fn does_not_delete_a_corrupt_newline_terminated_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = session_file(dir.path(), &[&user_entry("a", None), "{broken}\n"]);
        let original = std::fs::read(&path).unwrap();

        let result = SessionStore::open(&path);

        assert!(result.is_err(), "a complete corrupt record was silently discarded");
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn preserves_unknown_complete_records_even_without_a_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = session_file(dir.path(), &[&user_entry("a", None), r#"{"type":"future_kind"}"#]);
        let original = std::fs::read(&path).unwrap();

        assert!(SessionStore::open(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[tokio::test]
    async fn an_incomplete_utf8_suffix_does_not_hide_an_earlier_syntax_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invalid.jsonl");
        let mut store = SessionStore::create(&path, header()).unwrap();
        store.append_message(Message::user("keep")).await.unwrap();
        drop(store);
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{broken}\xe4\xb8")
            .unwrap();
        let original = std::fs::read(&path).unwrap();

        assert!(
            SessionStore::open(&path).is_err(),
            "invalid JSON was mistaken for an interrupted UTF-8 write"
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn new_session_files_are_private_to_the_owner() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private.jsonl");
        SessionStore::create(&path, header()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[tokio::test]
    async fn file_round_trip_and_recovery_from_partial_last_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        {
            let mut store = SessionStore::create(&path, header()).unwrap();
            store.append_message(Message::user("q")).await.unwrap();
            store.append_message(assistant("a")).await.unwrap();
        }
        assert!(
            SessionStore::create(&path, header()).is_err(),
            "never overwrites a session"
        );

        // Simulate a crash in the middle of writing a third entry.
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"type\":\"message\",\"id\":\"x").unwrap();
        drop(file);

        let mut store = SessionStore::open(&path).unwrap();
        assert_eq!(texts(&store), ["q", "a"]);
        store.append_message(Message::user("again")).await.unwrap();
        drop(store);
        let reopened = SessionStore::open(&path).unwrap();
        assert_eq!(texts(&reopened), ["q", "a", "again"]);
        assert_eq!(reopened.header().model, "m");
    }
}
