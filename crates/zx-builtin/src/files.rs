//! Bounded reads of regular files, opened without blocking so that a FIFO is refused rather
//! than waited on: synchronous for configuration at startup and for secret files before a
//! redaction (`redact`), asynchronous for the files that tools read (`LocalExecutor` and
//! `tools::paging`). [`read_text_file`] is public for zx-app, which reads its configuration
//! files with it; the module is not part of the API.

use std::fs::OpenOptions;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result, ensure};

/// Read a regular UTF-8 file of at most `max_bytes`: a small, bounded, synchronous read, as
/// for configuration at startup; errors name the path, and FIFOs and devices are refused
/// without waiting for a writer.
pub fn read_text_file(path: &Path, max_bytes: u64) -> Result<String> {
    let bytes = read_regular_file(path, max_bytes)?;
    String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", path.display()))
}

/// Read a regular file of at most `max_bytes`, as [`read_text_file`] does, without decoding
/// it: for a secret file before a redaction.
pub(crate) fn read_regular_file(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true);
    // Inspect the opened descriptor without blocking if a path is a FIFO.
    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK);
    let file = options
        .open(path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let meta = file
        .metadata()
        .with_context(|| format!("cannot inspect {}", path.display()))?;
    ensure!(meta.is_file(), "{} is not a regular file", path.display());
    ensure!(meta.len() <= max_bytes, "{} exceeds {max_bytes} bytes", path.display());
    let mut bytes = Vec::new();
    file.take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("cannot read {}", path.display()))?;
    ensure!(
        u64::try_from(bytes.len())? <= max_bytes,
        "{} exceeds {max_bytes} bytes",
        path.display()
    );
    Ok(bytes)
}

/// What opening a file does with a symbolic link in its place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Links {
    /// Follows it to the file it names.
    Follow,
    /// Refuses it: the open fails.
    Refuse,
}

/// The file at `path`, opened for reading without blocking, with the metadata of what was
/// opened, which tells the caller whether it is a regular file. A FIFO that was put at
/// `path` is opened at once instead of waiting for a writer, which would hold a blocking
/// thread, and so the runtime's shutdown, until one came.
pub(crate) async fn open_without_blocking(
    path: &Path,
    links: Links,
) -> std::io::Result<(tokio::fs::File, std::fs::Metadata)> {
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(match links {
        Links::Follow => libc::O_NONBLOCK,
        Links::Refuse => libc::O_NOFOLLOW | libc::O_NONBLOCK,
    });
    let file = options.open(path).await?;
    let meta = file.metadata().await?;
    Ok((file, meta))
}

/// At most `max_bytes` of `file`, and one byte more if it has one: metadata can be stale,
/// as for a growing file, so the caller tells an exact fit from a longer file by the length.
pub(crate) async fn read_bounded(file: tokio::fs::File, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1)).read_to_end(&mut bytes).await?;
    Ok(bytes)
}
