//! One JSONL session file: a header line, then one record per line.
//!
//! Its functions block on the file system: the storage runs them on a blocking thread
//! (`JsonlStorage::blocking`), and records reach the file on a writer thread of their own.
//!
//! - `tests`, compiled for tests only: the file's lock, reading it, repairing its end, and
//!   appending to it

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
#[cfg(unix)]
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use async_trait::async_trait;
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use zx_core::transcript::{LoadedSession, LogRecord, SessionHeader, SessionLog};

/// The longest header line read from a file that may not be a session at all.
const MAX_HEADER_BYTES: u64 = 1024 * 1024;

/// The header as the first line of a session file: tagged `session`, as an entry is tagged
/// with its type.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum HeaderLine {
    Session(SessionHeader),
}

/// `header` as the first line of a session file, without its newline.
fn header_line(header: &SessionHeader) -> Result<String> {
    Ok(serde_json::to_string(&HeaderLine::Session(header.clone()))?)
}

/// The header in `line`, the first line of the session file at `path` that is not blank;
/// `None` when the file has no such line.
fn header_in(line: Option<&[u8]>, path: &Path) -> Result<SessionHeader> {
    let line = line.ok_or_else(|| anyhow!("session file {} is empty", path.display()))?;
    let HeaderLine::Session(header) =
        serde_json::from_slice(line).with_context(|| format!("{}: invalid session header", path.display()))?;
    Ok(header)
}

/// Whether `line` holds nothing but whitespace.
fn is_blank(line: &[u8]) -> bool {
    line.iter().all(u8::is_ascii_whitespace)
}

/// What a failure to read the session file at `path` says.
fn cannot_read(path: &Path) -> String {
    format!("cannot read session file {}", path.display())
}

/// A session file open for appending. On Unix it holds an exclusive `flock(2)` lock until it
/// is dropped, so a second writer, in this process or another, cannot open the same file.
pub(super) struct JsonlFile {
    path: PathBuf,
    target: Target,
    /// The `seq` the next record must have.
    next_seq: u64,
}

/// Where the records of an open file go.
enum Target {
    /// The file as it was found: read and locked, not yet changed.
    Unrepaired(Unrepaired),
    Writer(Writer),
    /// The writer's thread could not be started, so the file takes no records.
    Unusable,
}

impl Target {
    /// The writer of the file, started at the first record. Before that record its thread
    /// repairs the file's end: by then the session has been loaded, which checked every other
    /// record, and the thread, not the runtime, waits for the file.
    fn writer(&mut self) -> Result<&mut Writer> {
        *self = match std::mem::replace(self, Self::Unusable) {
            Self::Unrepaired(unrepaired) => Self::Writer(Writer::spawn(move || unrepaired.repair())?),
            ready @ (Self::Writer(_) | Self::Unusable) => ready,
        };
        match self {
            Self::Writer(writer) => Ok(writer),
            Self::Unrepaired(_) | Self::Unusable => {
                bail!("the session writer could not be started, so the file takes no more records")
            }
        }
    }
}

/// Records reach the file on a thread of their own. A write to a volume that stopped
/// answering blocks inside the kernel; on that thread it stalls neither the runtime nor the
/// transcript's append timeout, and it does not keep the process from exiting.
struct Writer {
    lines: Option<mpsc::SyncSender<Line>>,
    thread: Option<thread::JoinHandle<()>>,
    /// Whether a line was sent and its result has not been received.
    pending: bool,
}

/// One record's line and where the result of writing it goes.
struct Line {
    text: String,
    written: oneshot::Sender<Result<()>>,
}

impl Writer {
    /// Start the thread that writes the lines. When the first line comes, the thread gets its
    /// target from `open`, such as the file once its end is repaired; when that fails, the
    /// first line reports why and nothing is written. A writer sent no line never calls it.
    fn spawn(open: impl FnOnce() -> Result<Box<dyn Write + Send>> + Send + 'static) -> Result<Self> {
        // One line at a time: the transcript waits for each append before the next one.
        let (lines, received) = mpsc::sync_channel::<Line>(1);
        let thread = thread::Builder::new()
            .name("zx-session-writer".into())
            .spawn(move || {
                let Ok(first) = received.recv() else {
                    return;
                };
                let mut destination = match open() {
                    Ok(destination) => destination,
                    Err(error) => {
                        // The transcript may have stopped waiting for this write; then no one
                        // needs its result.
                        let _ = first.written.send(Err(error));
                        return;
                    }
                };
                for line in std::iter::once(first).chain(received) {
                    let result = destination
                        .write_all(line.text.as_bytes())
                        .and_then(|()| destination.flush());
                    // As above: the transcript may have stopped waiting.
                    let _ = line.written.send(result.map_err(anyhow::Error::from));
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
        // With no write pending the channel is empty, so a line it refuses means the thread
        // is gone.
        ensure!(!self.pending, "an earlier write of the session file has not finished");
        let stopped = || anyhow!("the session writer has stopped");
        let lines = self.lines.as_ref().ok_or_else(stopped)?;
        let (written, result) = oneshot::channel();
        lines.try_send(Line { text, written }).map_err(|_| stopped())?;
        self.pending = true;
        let result = result
            .await
            .map_err(|_| anyhow!("the session writer stopped before the write finished"))?;
        self.pending = false;
        result
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
/// first; only [`Unrepaired::repair`], on the writer's thread, changes the file.
struct Unrepaired {
    path: PathBuf,
    file: File,
    repair: Repair,
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
        if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            crate::files::create_private_dir(parent)
                .with_context(|| format!("cannot create the session directory {}", parent.display()))?;
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
        let line = header_line(header)?;
        writeln!(file, "{line}")
            .and_then(|()| file.flush())
            .with_context(|| format!("cannot write the header of {}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            target: Target::Writer(Writer::spawn(move || Ok(Box::new(file)))?),
            next_seq: 1,
        })
    }

    /// Lock an existing file and read its header and records without changing it. An
    /// incomplete last line is left out, and repaired before the first record is appended
    /// ([`JsonlStorage`](super::JsonlStorage) says when it occurs).
    pub(super) fn open(path: &Path) -> Result<(LoadedSession, Self)> {
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(path)
            .with_context(|| format!("cannot open session file {}", path.display()))?;
        // Before reading: a concurrent writer's unfinished line must not look like a crash.
        lock(&file, path)?;
        let mut content = Vec::new();
        file.read_to_end(&mut content).with_context(|| cannot_read(path))?;
        let loaded = parse(&content, path)?;
        let repair = match loaded.incomplete_tail_bytes {
            0 if !content.is_empty() && !content.ends_with(b"\n") => Repair::Terminate,
            0 => Repair::Nothing,
            tail => Repair::Truncate(u64::try_from(content.len() - tail)?),
        };
        let opened = Self {
            path: path.to_path_buf(),
            target: Target::Unrepaired(Unrepaired {
                path: path.to_path_buf(),
                file,
                repair,
            }),
            next_seq: u64::try_from(loaded.records.len())? + 1,
        };
        Ok((loaded, opened))
    }

    /// Records of the file at `path` go to `writer`, so tests can make writes fail. No
    /// header is written.
    #[cfg(test)]
    pub(super) fn with_writer(path: &Path, writer: Box<dyn Write + Send + Sync>) -> Self {
        Self {
            path: path.to_path_buf(),
            target: Target::Writer(Writer::spawn(move || Ok(writer)).expect("tests can start a thread")),
            next_seq: 1,
        }
    }
}

/// Read a session file without locking or changing it. An incomplete last line, such as a
/// record that a writer has not finished, is left out.
pub(super) fn read(path: &Path) -> Result<LoadedSession> {
    let content = std::fs::read(path).with_context(|| cannot_read(path))?;
    parse(&content, path)
}

/// Read only the header of a session file, without locking or changing it. Blank lines
/// before it are skipped, as [`parse`] skips them.
pub(super) fn read_header(path: &Path) -> Result<SessionHeader> {
    let file = File::open(path).with_context(|| cannot_read(path))?;
    let mut lines = BufReader::new(file).take(MAX_HEADER_BYTES);
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = lines.read_until(b'\n', &mut line).with_context(|| cannot_read(path))?;
        if read == 0 || !is_blank(&line) {
            break;
        }
    }
    header_in((!is_blank(&line)).then_some(&line[..]), path)
}

/// The header and complete records of `content`, with the length of an incomplete last
/// line.
fn parse(content: &[u8], path: &Path) -> Result<LoadedSession> {
    // A short final write can end inside a UTF-8 character, so check one record at a
    // time rather than rejecting the whole file during UTF-8 decoding.
    let mut lines = Vec::new();
    let mut offset = 0;
    for raw in content.split_inclusive(|&byte| byte == b'\n') {
        if !is_blank(raw) {
            lines.push((offset, raw));
        }
        offset += raw.len();
    }
    let header = header_in(lines.first().map(|&(_, first)| first), path)?;

    let mut records = Vec::new();
    let mut incomplete = None;
    for (position, &(line_offset, raw)) in lines.iter().enumerate().skip(1) {
        if position + 1 == lines.len() && incomplete_tail(raw) {
            incomplete = Some(line_offset);
            break;
        }
        let record = std::str::from_utf8(raw).with_context(|| format!("{}: record {position}", path.display()))?;
        records.push(record.trim_end().to_string());
    }
    Ok(LoadedSession {
        header,
        records,
        incomplete_tail_bytes: incomplete.map_or(0, |start| content.len() - start),
    })
}

impl Unrepaired {
    /// Cut off an incomplete final record, or terminate a complete one that lacks its
    /// newline, so that the next record starts on a line of its own. Returns the file.
    fn repair(self) -> Result<Box<dyn Write + Send>> {
        let Self { path, file, repair } = self;
        match repair {
            Repair::Truncate(length) => file.set_len(length),
            Repair::Terminate => (&file).write_all(b"\n"),
            Repair::Nothing => Ok(()),
        }
        .with_context(|| format!("cannot repair session file {}", path.display()))?;
        Ok(Box::new(file))
    }
}

#[async_trait]
impl SessionLog for JsonlFile {
    async fn append(&mut self, record: &LogRecord<'_>) -> Result<()> {
        record
            .check_seq(self.next_seq)
            .with_context(|| format!("session file {}", self.path.display()))?;
        // The line survives a crash of this process, not of the machine: there is no fsync.
        let mut line = String::with_capacity(record.json.len() + 1);
        line.push_str(record.json);
        line.push('\n');
        self.target
            .writer()?
            .write(line)
            .await
            .with_context(|| format!("cannot write session file {}", self.path.display()))?;
        self.next_seq += 1;
        Ok(())
    }
}

/// How long opening keeps trying a lock that another descriptor holds before reporting the file
/// as in use. A program that this process is starting holds copies of its descriptors until it
/// runs, so a transcript that was just dropped can keep its lock for a moment: under a millisecond
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
                std::thread::sleep(pause);
                pause = (pause * 2).min(LOCK_RETRY_MAX);
            }
            std::io::ErrorKind::WouldBlock => bail!(
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
mod tests;
