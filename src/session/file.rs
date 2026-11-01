//! The JSONL session file: a header line, then one record per line.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, ensure};
use async_trait::async_trait;
use serde::de::IgnoredAny;

use super::log::{LoadedSession, LogRecord, SessionLog};
use super::{HeaderLine, SessionHeader};

/// A session file open for appending. On Unix it holds an exclusive `flock(2)` lock until it
/// is dropped, so a second writer, in this process or another, cannot open the same file.
pub(super) struct JsonlFile {
    writer: Box<dyn Write + Send + Sync>,
    /// The `seq` the next record must have.
    next_seq: u64,
}

/// A locked session file that has been read but not yet changed. Its records are checked
/// first; only [`Unrepaired::repair`] changes the file and allows appending.
pub(super) struct Unrepaired {
    path: PathBuf,
    file: File,
    /// Where an incomplete final record starts.
    truncate_at: Option<u64>,
    /// Whether the last complete record lacks its newline.
    terminate: bool,
    next_seq: u64,
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
        writeln!(file, "{}", serde_json::to_string(&HeaderLine::Session(header.clone()))?)?;
        file.flush()?;
        Ok(Self {
            writer: Box::new(file),
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
        let unrepaired = Unrepaired {
            path: path.to_path_buf(),
            file,
            truncate_at: truncate_at.map(u64::try_from).transpose()?,
            terminate: truncate_at.is_none() && !content.is_empty() && !content.ends_with(b"\n"),
            next_seq: u64::try_from(loaded.records.len())? + 1,
        };
        Ok((loaded, unrepaired))
    }

    /// Records go to `writer`, so tests can make writes fail. No header is written.
    #[cfg(test)]
    pub(super) fn with_writer(writer: Box<dyn Write + Send + Sync>) -> Self {
        Self { writer, next_seq: 1 }
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
            truncate_at,
            terminate,
            next_seq,
        } = self;
        if let Some(length) = truncate_at {
            file.set_len(length)
                .with_context(|| format!("cannot repair session file {}", path.display()))?;
        } else if terminate {
            (&file)
                .write_all(b"\n")
                .with_context(|| format!("cannot repair session file {}", path.display()))?;
        }
        Ok(JsonlFile {
            writer: Box::new(file),
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
        // Synchronous I/O inside an async method: one short line per record reaches the
        // operating system in microseconds, and the store waits for it before going on.
        // It survives a crash of this process, not of the machine: there is no fsync.
        let mut line = String::with_capacity(record.json.len() + 1);
        line.push_str(record.json);
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        self.writer.flush()?;
        self.next_seq += 1;
        Ok(())
    }
}

/// Take an exclusive lock on the whole file without waiting. It lasts until the file is
/// closed, including when the process exits or is killed.
#[cfg(unix)]
fn lock(file: &File, path: &Path) -> Result<()> {
    use std::os::fd::AsRawFd;

    // SAFETY: flock(2) has no memory-safety preconditions; the descriptor stays open for the
    // call because `file` is borrowed.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        anyhow::bail!(
            "session file {} is in use: another process or session has it open for writing",
            path.display()
        );
    }
    Err(error).with_context(|| format!("cannot lock session file {}", path.display()))
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
