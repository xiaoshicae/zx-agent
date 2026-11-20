//! Bounded reads of regular files, opened without blocking so that a FIFO is refused rather
//! than waited on: blocking reads for configuration at startup and for secret files before a
//! redaction (`redact`), and the same read on a blocking thread for the files that tools read
//! (`LocalExecutor` and `tools::paging`); and the directories only this user may read, for
//! what other users must not see. [`read_text_blocking`] is public for zx-app, which reads its
//! configuration files with it, [`create_private_dir`] for zx-im, which keeps its messages in
//! such directories, and [`Links`] for the tools of hosts, as `tools::paging` re-exports it;
//! the module is not part of the API.

use std::fs::{File, Metadata, OpenOptions};
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result, anyhow};

/// Create `directory` and its missing parents so that on Unix only this user may read, list
/// or enter them, as for session files, which hold the conversation and tool output. A
/// directory that exists keeps its permissions.
pub fn create_private_dir(directory: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(directory)
}

/// What opening a file does with a symbolic link in its place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Links {
    /// Follows it to the file it names.
    Follow,
    /// Refuses it: the open fails.
    Refuse,
}

/// Why a file was not read. The tools word it for the model themselves; [`Self::describe`]
/// words it for a person.
#[derive(Debug)]
pub(crate) enum Unreadable {
    /// Inspecting, opening or reading it failed.
    Io(std::io::Error),
    /// A symbolic link is in its place, and links are refused.
    Link,
    /// It is a directory.
    Directory,
    /// It is neither a regular file nor a directory, such as a FIFO or a device.
    Special,
    /// It is larger than the limit.
    Over,
}

impl Unreadable {
    /// Why the file at `path` was not read, in words for a person, as configuration and secret
    /// files are reported.
    pub(crate) fn describe(&self, path: &Path, max_bytes: u64) -> String {
        let path = path.display();
        match self {
            Self::Io(error) => format!("cannot read {path}: {error}"),
            Self::Link | Self::Directory | Self::Special => format!("{path} is not a regular file"),
            Self::Over => format!("{path} exceeds {max_bytes} bytes"),
        }
    }
}

/// Read a regular UTF-8 file of at most `max_bytes`, as for configuration at startup: a
/// blocking read with the checks of `read_regular_blocking`, whose error names the path in
/// words for a person.
pub fn read_text_blocking(path: &Path, max_bytes: u64) -> Result<String> {
    let bytes = read_regular_blocking(path, max_bytes, Links::Follow)
        .map_err(|unreadable| anyhow!(unreadable.describe(path, max_bytes)))?;
    String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", path.display()))
}

/// [`read_regular_blocking`] on a blocking thread, for the files that tools read. A dropped
/// future leaves the read to end there, which it soon does: it is bounded, and it waits for
/// no writer.
pub(crate) async fn read_regular(path: &Path, max_bytes: u64, links: Links) -> Result<Vec<u8>, Unreadable> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || read_regular_blocking(&path, max_bytes, links))
        .await
        .map_err(|error| Unreadable::Io(std::io::Error::other(error)))?
}

/// The bytes of the regular file at `path`, at most `max_bytes` of them, with a symbolic link
/// in its place followed or refused as `links` says: a blocking read, small and bounded.
///
/// Opening a special file can act by itself: it releases the writer of a FIFO, and some
/// devices start working. So only what was found to be a regular file is opened, and what
/// the open got is checked again, as another file may have taken its place since
/// ([`open_regular`]). The metadata can be stale as well, as for a file that grows while it
/// is read, so the read also stops at the limit.
pub(crate) fn read_regular_blocking(path: &Path, max_bytes: u64, links: Links) -> Result<Vec<u8>, Unreadable> {
    let found = match links {
        Links::Follow => std::fs::metadata(path),
        Links::Refuse => std::fs::symlink_metadata(path),
    }
    .map_err(Unreadable::Io)?;
    check_regular(&found)?;
    let (file, opened) = open_regular(path, links)?;
    if opened.len() > max_bytes {
        return Err(Unreadable::Over);
    }
    let mut reader = file.take(max_bytes.saturating_add(1));
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).map_err(Unreadable::Io)?;
    // The reader may go one byte past the limit, which only a longer file has.
    if reader.limit() == 0 {
        return Err(Unreadable::Over);
    }
    Ok(bytes)
}

/// The file at `path`, opened for reading, with its metadata, if it is a regular file. The
/// open does not block: a FIFO put at `path` is opened at once, and refused, instead of
/// waiting for a writer, which would hold a blocking thread, and so the runtime's shutdown,
/// until one came.
fn open_regular(path: &Path, links: Links) -> Result<(File, Metadata), Unreadable> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(match links {
        Links::Follow => libc::O_NONBLOCK,
        Links::Refuse => libc::O_NOFOLLOW | libc::O_NONBLOCK,
    });
    let file = options.open(path).map_err(Unreadable::Io)?;
    let meta = file.metadata().map_err(Unreadable::Io)?;
    check_regular(&meta)?;
    Ok((file, meta))
}

/// Refuse what `meta` describes unless it is a regular file.
fn check_regular(meta: &Metadata) -> Result<(), Unreadable> {
    let kind = meta.file_type();
    if kind.is_file() {
        Ok(())
    } else if kind.is_symlink() {
        Err(Unreadable::Link)
    } else if kind.is_dir() {
        Err(Unreadable::Directory)
    } else {
        Err(Unreadable::Special)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::test_support::make_fifo;

    #[tokio::test]
    async fn a_fifo_in_place_of_a_checked_file_is_refused_without_waiting_for_a_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fifo");
        make_fifo(&path);
        let fifo = path.clone();

        // An open that waited for a writer would never end by itself: nothing writes.
        let opened = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::task::spawn_blocking(move || open_regular(&fifo, Links::Follow)),
        )
        .await;
        // Release such an open, so that a failing test does not hold the runtime. Opening the
        // write end fails when nothing waits on the read end: then there is nothing to release.
        std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .ok();

        let refused = opened.expect("the open waited for a writer").unwrap().unwrap_err();
        assert!(matches!(refused, Unreadable::Special), "{refused:?}");
    }
}
