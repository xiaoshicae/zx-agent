//! What chat's text mode draws on the terminal: colors, and where the cursor is, so that
//! every line starts on a line of its own.
//!
//! Answers go to stdout, everything else to stderr. Both usually share one terminal, so one
//! bookkeeping covers them; when they are separate, the worst case is a spare blank line.

use std::io::Write;
use std::time::Duration;

/// How a line is colored, when colors are on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Tone {
    /// Model answers and anything a user must read as is.
    Plain,
    /// Progress and housekeeping.
    Dim,
    /// Something to read carefully, such as a call awaiting approval.
    Strong,
    /// A call that succeeded, or a line a patch adds.
    Ok,
    /// Approvals and recoverable problems.
    Warn,
    /// Failures, or a line a patch removes.
    Error,
    /// The input prompt.
    Prompt,
}

#[derive(Clone, Copy)]
pub(super) struct Style {
    color: bool,
}

impl Style {
    pub(super) fn new(color: bool) -> Self {
        Self { color }
    }

    /// Colors go only to an interactive terminal that is not `dumb`, and never when `NO_COLOR`
    /// is set to a non-empty value (<https://no-color.org>).
    pub(super) fn auto(stderr_is_terminal: bool) -> Self {
        let color = stderr_is_terminal
            && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
            && std::env::var_os("TERM").is_none_or(|value| value != "dumb");
        Self { color }
    }

    pub(super) fn paint(&self, tone: Tone, text: &str) -> String {
        let code = match tone {
            Tone::Plain => return text.to_string(),
            Tone::Dim => "2",
            Tone::Strong | Tone::Prompt => "1",
            Tone::Ok => "32",
            // Bold as well: plain yellow is faint on a light background.
            Tone::Warn => "1;33",
            Tone::Error => "31",
        };
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
}

pub(super) struct Terminal {
    style: Style,
    /// Blank lines around answers only on an interactive stdout.
    spaced: bool,
    /// The stream whose last write left its line unfinished.
    open_line: Option<Stream>,
    last_line_blank: bool,
    /// An answer's text is being streamed.
    in_answer: bool,
}

impl Terminal {
    pub(super) fn new(style: Style, spaced: bool) -> Self {
        Self {
            style,
            spaced,
            open_line: None,
            last_line_blank: false,
            in_answer: false,
        }
    }

    pub(super) fn style(&self) -> Style {
        self.style
    }

    /// A piece of the answer, on stdout. The first piece after [`Self::end_answer`] starts
    /// on a fresh line, after a blank one on an interactive terminal.
    pub(super) fn answer_text(&mut self, text: &str) {
        if !self.in_answer {
            self.in_answer = true;
            self.ensure_line_start();
            if self.spaced {
                self.ensure_blank_line(Stream::Stdout);
            }
        }
        self.write(Stream::Stdout, text);
    }

    /// The answer is complete: end its line, and leave a blank one on an interactive terminal.
    pub(super) fn end_answer(&mut self) {
        if !self.in_answer {
            return;
        }
        self.in_answer = false;
        self.ensure_line_start();
        if self.spaced {
            self.ensure_blank_line(Stream::Stdout);
        }
    }

    /// One line on stderr, on a line of its own.
    pub(super) fn line(&mut self, tone: Tone, text: &str) {
        self.ensure_line_start();
        let painted = self.style.paint(tone, text);
        self.write(Stream::Stderr, &format!("{painted}\n"));
    }

    /// A prompt on stderr, without a line ending.
    pub(super) fn prompt(&mut self, text: &str) {
        self.ensure_line_start();
        let painted = self.style.paint(Tone::Prompt, text);
        self.write(Stream::Stderr, &painted);
    }

    /// Text shown after the prompt as if the user had typed it, ending the line.
    pub(super) fn echo(&mut self, text: &str) {
        self.write(Stream::Stderr, &format!("{text}\n"));
    }

    /// The terminal echoed a line the user typed, so the cursor is at the start of a new
    /// line, below whatever was drawn.
    pub(super) fn input_echoed(&mut self) {
        self.open_line = None;
        self.last_line_blank = false;
    }

    /// End an open line, for example before the process exits.
    pub(super) fn finish(&mut self) {
        self.ensure_line_start();
    }

    /// End an unfinished line on the stream that left it so, which keeps a piped stdout
    /// newline-terminated.
    fn ensure_line_start(&mut self) {
        if let Some(stream) = self.open_line {
            self.write(stream, "\n");
        }
    }

    fn ensure_blank_line(&mut self, stream: Stream) {
        if !self.last_line_blank {
            self.write(stream, "\n");
        }
    }

    fn write(&mut self, stream: Stream, text: &str) {
        if text.is_empty() {
            return;
        }
        let was_at_line_start = self.open_line.is_none();
        // A closed stdout or stderr leaves nobody to show more to; prompts still return a report.
        let _ = match stream {
            Stream::Stdout => {
                let mut stdout = std::io::stdout().lock();
                stdout.write_all(text.as_bytes()).and_then(|()| stdout.flush())
            }
            Stream::Stderr => {
                let mut stderr = std::io::stderr().lock();
                stderr.write_all(text.as_bytes()).and_then(|()| stderr.flush())
            }
        };
        self.open_line = if text.ends_with('\n') { None } else { Some(stream) };
        self.last_line_blank = text.ends_with("\n\n") || (text == "\n" && was_at_line_start);
    }
}

#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

/// `1.2s` under ten seconds, whole seconds beyond.
pub(super) fn elapsed_text(duration: Duration) -> String {
    let secs = duration.as_secs_f64();
    if secs < 10.0 {
        format!("{secs:.1}s")
    } else {
        format!("{secs:.0}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_wrap_text_only_when_enabled() {
        let on = Style { color: true };
        let off = Style { color: false };
        assert_eq!(on.paint(Tone::Warn, "x"), "\x1b[1;33mx\x1b[0m");
        assert_eq!(on.paint(Tone::Ok, "x"), "\x1b[32mx\x1b[0m");
        assert_eq!(on.paint(Tone::Plain, "x"), "x");
        assert_eq!(off.paint(Tone::Warn, "x"), "x");
    }

    #[test]
    fn elapsed_shows_tenths_only_under_ten_seconds() {
        assert_eq!(elapsed_text(Duration::from_millis(1234)), "1.2s");
        assert_eq!(elapsed_text(Duration::from_millis(12_345)), "12s");
    }
}
