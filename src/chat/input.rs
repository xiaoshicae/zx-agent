//! One process-lifetime input owner, as recommended by Tokio's stdin documentation.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};

use anyhow::{Context, Result};
use tokio::sync::mpsc;

use super::InputMode;

/// Bound both read-ahead and the size of one input line (including its line ending). Lines
/// pasted together are joined up to the same size; what is left starts the next message.
pub(super) const MAX_LINE_BYTES: usize = 1024 * 1024;
const INPUT_QUEUE: usize = 8;

/// One message from the input owner.
pub(super) enum Line {
    /// A complete line without its line ending, or several lines that arrived together.
    Text(String),
    /// A line that was read in full but not delivered, with the reason.
    Rejected(String),
}

pub(super) type Input = mpsc::Receiver<io::Result<Line>>;

pub(super) fn spawn(mode: InputMode) -> Result<Input> {
    // A handle of its own: only this thread reads stdin, so the global lock's buffer would hide
    // whether more input is waiting.
    let stdin = stdin_handle().context("cannot open terminal input")?;
    let (sender, receiver) = mpsc::channel(INPUT_QUEUE);
    let thread = std::thread::Builder::new()
        .name("zx-chat-input".into())
        .spawn(move || {
            let mut reader = Reader {
                inner: BufReader::new(stdin),
                carried: None,
            };
            loop {
                let line = match read_message(&mut reader, mode) {
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

fn stdin_handle() -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;
        Ok(File::from(io::stdin().as_fd().try_clone_to_owned()?))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsHandle;
        Ok(File::from(io::stdin().as_handle().try_clone_to_owned()?))
    }
}

struct Reader {
    inner: BufReader<File>,
    /// A line read while joining a paste that did not fit the message; it starts the next one.
    carried: Option<String>,
}

/// The next message; `None` at EOF. In a terminal, lines that are already waiting when a line
/// completes were pasted together and form one message, unless the first is a slash command.
fn read_message(reader: &mut Reader, mode: InputMode) -> io::Result<Option<String>> {
    let first = match reader.carried.take() {
        Some(line) => Some(line),
        None => read_line(&mut reader.inner)?,
    };
    let Some(mut text) = first else {
        return Ok(None);
    };
    if mode == InputMode::Terminal && !text.trim_start().starts_with('/') {
        while input_waiting(&mut reader.inner) {
            let Some(next) = read_line(&mut reader.inner)? else {
                break;
            };
            if text.len() + 1 + next.len() > MAX_LINE_BYTES {
                reader.carried = Some(next);
                break;
            }
            text.push('\n');
            text.push_str(&next);
        }
    }
    Ok(Some(text))
}

/// Whether another line can be read without blocking. A terminal in its usual mode delivers
/// whole lines, so this is true only for lines that arrived together, such as a paste.
#[cfg(unix)]
fn input_waiting(reader: &mut BufReader<File>) -> bool {
    use std::os::fd::AsRawFd;

    if !reader.buffer().is_empty() {
        return true;
    }
    let mut poll = libc::pollfd {
        fd: reader.get_ref().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `poll` reads one initialized pollfd that outlives the call; a zero timeout returns
    // at once. The descriptor stays open, owned by `reader`.
    let ready = unsafe { libc::poll(&mut poll, 1, 0) };
    ready > 0 && poll.revents & libc::POLLIN != 0
}

#[cfg(not(unix))]
fn input_waiting(_reader: &mut BufReader<File>) -> bool {
    false
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

    #[cfg(unix)]
    mod pasted {
        use std::io::Write;
        use std::os::fd::OwnedFd;

        use super::*;

        fn pipe() -> (Reader, io::PipeWriter) {
            let (reader, writer) = io::pipe().unwrap();
            let reader = Reader {
                inner: BufReader::new(File::from(OwnedFd::from(reader))),
                carried: None,
            };
            (reader, writer)
        }

        #[test]
        fn lines_that_arrived_together_form_one_message_in_a_terminal() {
            let (mut reader, mut writer) = pipe();
            writer.write_all("第一行\n第二行\nthird\n".as_bytes()).unwrap();
            assert_eq!(
                read_message(&mut reader, InputMode::Terminal).unwrap().as_deref(),
                Some("第一行\n第二行\nthird")
            );
            writer.write_all(b"later\n").unwrap();
            assert_eq!(
                read_message(&mut reader, InputMode::Terminal).unwrap().as_deref(),
                Some("later")
            );
            drop(writer);
            assert!(read_message(&mut reader, InputMode::Terminal).unwrap().is_none());
        }

        #[test]
        fn a_script_consumes_one_line_per_message() {
            let (mut reader, mut writer) = pipe();
            writer.write_all(b"one\ntwo\n").unwrap();
            assert_eq!(
                read_message(&mut reader, InputMode::Script).unwrap().as_deref(),
                Some("one")
            );
            assert_eq!(
                read_message(&mut reader, InputMode::Script).unwrap().as_deref(),
                Some("two")
            );
        }

        #[test]
        fn a_joined_message_stays_within_the_line_limit() {
            let (mut reader, mut writer) = pipe();
            let big = "x".repeat(MAX_LINE_BYTES / 2 + 1);
            let thread = std::thread::spawn(move || {
                writer.write_all(format!("{big}\n{big}\nrest\n").as_bytes()).unwrap();
            });
            let first = read_message(&mut reader, InputMode::Terminal).unwrap().unwrap();
            assert_eq!(first.len(), MAX_LINE_BYTES / 2 + 1);
            let second = read_message(&mut reader, InputMode::Terminal).unwrap().unwrap();
            assert_eq!(
                second.len(),
                MAX_LINE_BYTES / 2 + 1 + 5,
                "the carried line heads the next message"
            );
            assert!(second.ends_with("\nrest"));
            thread.join().unwrap();
        }

        #[test]
        fn a_slash_command_is_never_joined_with_the_next_line() {
            let (mut reader, mut writer) = pipe();
            writer.write_all(b"/model next\nsecond\n").unwrap();
            assert_eq!(
                read_message(&mut reader, InputMode::Terminal).unwrap().as_deref(),
                Some("/model next")
            );
            assert_eq!(
                read_message(&mut reader, InputMode::Terminal).unwrap().as_deref(),
                Some("second")
            );
        }
    }
}
