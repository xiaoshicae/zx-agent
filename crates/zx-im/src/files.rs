//! What the program's stores on disk share, the statuses of runs (`StatusFiles`) and the
//! backlog (`BacklogFiles`): one record per file, named after what it is about and written so
//! that a crash never leaves part of one, read and written off the runtime.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

/// The extension of a record file; a record is written under another name first.
pub(crate) const RECORD: &str = "json";
const PARTIAL: &str = "partial";

/// `id` as one part of a file name, encoded so that no ID can name another file: bytes other
/// than ASCII letters, digits, `-` and `_` become `%XX`, and an empty part becomes `%`.
pub(crate) fn segment(id: &str) -> String {
    if id.is_empty() {
        return "%".to_string();
    }
    let mut encoded = String::with_capacity(id.len());
    for byte in id.bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Run `work`, which reads or writes the files of `what`, off the runtime: a volume that
/// stopped answering then stalls neither the runtime nor the timeout of the call.
pub(crate) async fn blocking<T: Send + 'static>(
    what: &str,
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| format!("reading or writing {what} stopped: {error}"))?
}

/// Create the file `path`, or empty it, so that on Unix only the owner may read it, as with
/// session files: which chats and threads the server answers in, and what people say there,
/// is not for other users to read.
pub(crate) fn create_private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

/// Write `record` to `path` as JSON, under a temporary name first, then rename it, so that a
/// crash never leaves part of a record. Only the owner may read it ([`create_private_file`]).
pub(crate) fn write_record(path: &Path, record: &impl Serialize) -> Result<(), String> {
    let text = serde_json::to_string(record)
        .map_err(|error| format!("cannot encode the record of {}: {error}", path.display()))?;
    let partial = path.with_extension(PARTIAL);
    create_private_file(&partial)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .map_err(|error| format!("cannot write {}: {error}", partial.display()))?;
    if let Err(error) = std::fs::rename(&partial, path) {
        // The record failed either way. Should the removal fail too, the file stays, and
        // readers pass over it.
        std::fs::remove_file(&partial).ok();
        return Err(format!(
            "cannot rename {} to {}: {error}",
            partial.display(),
            path.display()
        ));
    }
    Ok(())
}

/// Remove the record in `path`; one that is not there is removed already.
pub(crate) fn remove_record(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot remove {}: {error}", path.display())),
    }
}

/// The record in `path`, a record of `what` (for the error).
pub(crate) fn read_record<T: DeserializeOwned>(path: &Path, what: &str) -> Result<T, String> {
    let text = std::fs::read_to_string(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    serde_json::from_str(&text).map_err(|error| format!("{} is not a {what} record: {error}", path.display()))
}

/// The records in `dir`, the files with the extension [`RECORD`], in the order of their
/// names: anything else is a write that a crash cut short before its rename.
pub(crate) fn record_paths(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let unreadable = |error: io::Error| format!("cannot read {}: {error}", dir.display());
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(unreadable)? {
        let path = entry.map_err(unreadable)?.path();
        if path.extension().is_some_and(|extension| extension == RECORD) {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_part_of_a_file_name_cannot_name_another_file() {
        for (id, encoded) in [
            ("C0123", "C0123"),
            ("..", "%2E%2E"),
            ("a/b", "a%2Fb"),
            ("1700000000.000100", "1700000000%2E000100"),
            ("群", "%E7%BE%A4"),
            ("", "%"),
        ] {
            assert_eq!(segment(id), encoded, "{id:?}");
        }
    }
}
