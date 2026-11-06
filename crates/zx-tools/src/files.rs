//! Bounded, synchronous reads of small regular files, for configuration at startup and
//! secret files before a redaction. The text reader is shared with the CLI; not part of the
//! API.

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
