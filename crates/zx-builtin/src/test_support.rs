//! What the tests of several modules share: FIFOs, which the reads of files refuse instead of
//! waiting for a writer.

use std::path::Path;

/// Make a FIFO at `path`.
pub(crate) fn make_fifo(path: &Path) {
    let status = std::process::Command::new("mkfifo").arg(path).status().unwrap();
    assert!(status.success(), "mkfifo {}: {status}", path.display());
}
