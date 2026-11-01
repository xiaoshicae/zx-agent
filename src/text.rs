//! Bounding text for errors, messages and terminal output, and reading small text files.
//! Shared by the kernel and the CLI, and depends on nothing else in the crate.

use std::fs::OpenOptions;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result, ensure};

/// Read a regular UTF-8 file of at most `max_bytes`. The read is small, bounded and
/// synchronous, as for configuration at startup or a secret file before a redaction; errors
/// name the path. FIFOs and devices are refused without waiting for a writer.
pub fn read_text_file(path: &Path, max_bytes: u64) -> Result<String> {
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
    String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", path.display()))
}

/// The first `max_chars` characters of `text`, followed by "..." when more were left out.
pub fn clip(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((index, _)) => format!("{}...", &text[..index]),
        None => text.to_string(),
    }
}

/// The longest start of `text` that fits in `max_bytes` and ends on a character boundary.
pub fn prefix_within(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_counts_characters_and_marks_what_it_leaves_out() {
        assert_eq!(clip("abc", 3), "abc");
        assert_eq!(clip("界面ab", 2), "界面...");
    }

    #[test]
    fn a_prefix_never_splits_a_character() {
        assert_eq!(prefix_within("abc", 10), "abc");
        // "界" is three bytes: four bytes hold one of them, not one and a third.
        assert_eq!(prefix_within("界界", 4), "界");
        assert_eq!(prefix_within("界", 2), "");
    }
}
