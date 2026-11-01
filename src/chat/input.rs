//! One process-lifetime input owner, as recommended by Tokio's stdin documentation.

use std::io::{self, BufRead, Read};

use anyhow::{Context, Result};
use tokio::sync::mpsc;

/// Bound both read-ahead and the size of one input line (including its line ending).
pub(super) const MAX_LINE_BYTES: usize = 1024 * 1024;
const INPUT_QUEUE: usize = 8;

/// One line from the input owner.
pub(super) enum Line {
    /// A complete line without its line ending.
    Text(String),
    /// A line that was read in full but not delivered, with the reason.
    Rejected(String),
}

pub(super) type Input = mpsc::Receiver<io::Result<Line>>;

pub(super) fn spawn() -> Result<Input> {
    let (sender, receiver) = mpsc::channel(INPUT_QUEUE);
    let thread = std::thread::Builder::new()
        .name("zx-chat-input".into())
        .spawn(move || {
            let stdin = io::stdin();
            let mut reader = stdin.lock();
            loop {
                let line = match read_line(&mut reader) {
                    Ok(Some(text)) => Ok(Line::Text(text)),
                    Ok(None) => break,
                    // An oversized or non-UTF-8 line has been consumed whole; later lines are intact.
                    Err(error) if error.kind() == io::ErrorKind::InvalidData => Ok(Line::Rejected(error.to_string())),
                    Err(error) => Err(error),
                };
                let failed = line.is_err();
                if sender.blocking_send(line).is_err() {
                    // The frontend exited, so nobody needs further input.
                    break;
                }
                if failed {
                    break;
                }
            }
        })
        .context("cannot start terminal input reader")?;
    // A terminal read cannot be cancelled. This dedicated OS thread lives until EOF
    // or process exit; it is not a Tokio blocking task that holds runtime shutdown open.
    drop(thread);
    Ok(receiver)
}

/// The next line without its line ending; `None` at EOF. Oversized and non-UTF-8 lines are
/// consumed whole and reported as `InvalidData`, so the following line starts intact.
fn read_line(reader: &mut impl BufRead) -> io::Result<Option<String>> {
    let mut bytes = Vec::new();
    let limit = u64::try_from(MAX_LINE_BYTES).expect("one MiB fits in u64") + 1;
    let read = reader.by_ref().take(limit).read_until(b'\n', &mut bytes)?;
    if read == 0 {
        return Ok(None);
    }
    if bytes.len() > MAX_LINE_BYTES {
        // Skip the rest of the line without buffering it.
        if bytes.last() != Some(&b'\n') {
            reader.skip_until(b'\n')?;
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("input line exceeds {MAX_LINE_BYTES} bytes and was discarded"),
        ));
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_unicode_crlf_empty_lines_and_a_final_line_without_newline() {
        let mut input = io::Cursor::new("你好\r\n\n尾行".as_bytes());
        assert_eq!(read_line(&mut input).unwrap().as_deref(), Some("你好"));
        assert_eq!(read_line(&mut input).unwrap().as_deref(), Some(""));
        assert_eq!(read_line(&mut input).unwrap().as_deref(), Some("尾行"));
        assert!(read_line(&mut input).unwrap().is_none());
    }

    #[test]
    fn accepts_the_exact_byte_limit() {
        let data = vec![b'x'; MAX_LINE_BYTES];
        assert_eq!(
            read_line(&mut io::Cursor::new(data)).unwrap().unwrap().len(),
            MAX_LINE_BYTES
        );
    }

    #[test]
    fn rejects_oversized_input_instead_of_sending_a_truncated_character() {
        let data = format!("{}界\n", "x".repeat(MAX_LINE_BYTES - 1));
        let error = read_line(&mut io::Cursor::new(data)).unwrap_err();
        assert!(error.to_string().contains("exceeds"));
    }

    #[test]
    fn an_oversized_line_is_discarded_whole_so_the_next_line_is_intact() {
        let data = format!("{}\nnext\n", "x".repeat(MAX_LINE_BYTES + 5));
        let mut input = io::Cursor::new(data.into_bytes());
        assert!(read_line(&mut input).unwrap_err().to_string().contains("exceeds"));
        assert_eq!(read_line(&mut input).unwrap().as_deref(), Some("next"));
    }

    #[test]
    fn invalid_utf8_is_an_input_error() {
        assert_eq!(
            read_line(&mut io::Cursor::new([0xff, b'\n'])).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
