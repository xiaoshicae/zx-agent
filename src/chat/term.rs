//! What chat's text mode draws on the terminal: colors, the transient status line and where
//! the cursor is, so that every line starts on a line of its own.
//!
//! Answers go to stdout, everything else to stderr. Both usually share one terminal, so one
//! bookkeeping covers them; when they are separate, the worst case is a spare blank line.

use std::io::Write;
use std::time::{Duration, Instant};

/// One frame per tick.
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
/// The widest status line when the terminal's width cannot be asked: under the usual 80
/// columns, because a wrapped line cannot be redrawn with a carriage return.
const STATUS_FALLBACK_CHARS: usize = 78;
/// A status line keeps at least this much of its text, whatever the terminal says.
const STATUS_MIN_CHARS: usize = 20;
/// A status is drawn once it has lasted this long, so short waits leave no trace.
const STATUS_DELAY: Duration = Duration::from_secs(1);

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

    pub(super) fn color(&self) -> bool {
        self.color
    }

    pub(super) fn paint(&self, tone: Tone, text: &str) -> String {
        let code = match tone {
            Tone::Plain => return text.to_string(),
            Tone::Dim => "2",
            Tone::Strong => "1",
            Tone::Ok => "32",
            // Bold as well: plain yellow is faint on a light background.
            Tone::Warn => "1;33",
            Tone::Error => "31",
            Tone::Prompt => "1",
        };
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
}

/// What the run is doing while nothing else is printed.
#[derive(Clone, PartialEq, Eq)]
pub(super) enum Phase {
    /// A model request is out; `thinking` once reasoning streams in.
    Model { thinking: bool },
    /// A tool runs; the label names it and its main argument.
    Tool { label: String },
}

struct StatusLine {
    phase: Phase,
    since: Instant,
    /// Characters on the row the status occupies; 0 while it is not drawn yet.
    drawn_chars: usize,
    frame: usize,
}

pub(super) struct Terminal {
    style: Style,
    /// Status lines are drawn only on an interactive stderr.
    live: bool,
    /// Blank lines around answers only on an interactive stdout.
    spaced: bool,
    status: Option<StatusLine>,
    /// The stream whose last write left its line unfinished.
    open_line: Option<Stream>,
    last_line_blank: bool,
    /// An answer's text is being streamed.
    in_answer: bool,
}

impl Terminal {
    pub(super) fn new(style: Style, live: bool, spaced: bool) -> Self {
        Self {
            style,
            live,
            spaced,
            status: None,
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
        // The model is answering: the wait is over.
        self.clear_status();
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
        self.settle_status();
        self.ensure_line_start();
        let painted = self.style.paint(tone, text);
        self.write(Stream::Stderr, &format!("{painted}\n"));
    }

    /// A prompt on stderr, without a line ending.
    pub(super) fn prompt(&mut self, text: &str) {
        self.settle_status();
        self.ensure_line_start();
        let painted = self.style.paint(Tone::Prompt, text);
        self.write(Stream::Stderr, &painted);
    }

    /// Text shown after the prompt as if the user had typed it, ending the line.
    pub(super) fn echo(&mut self, text: &str) {
        self.settle_status();
        self.write(Stream::Stderr, &format!("{text}\n"));
    }

    /// The terminal echoed a line the user typed, so the cursor is at the start of a new
    /// line, below whatever was drawn.
    pub(super) fn input_echoed(&mut self) {
        self.open_line = None;
        self.last_line_blank = false;
        if let Some(status) = &mut self.status {
            // The status row is above the echo now; the next tick draws a fresh one.
            status.drawn_chars = 0;
        }
    }

    /// End an open line, for example before the process exits.
    pub(super) fn finish(&mut self) {
        self.settle_status();
        self.ensure_line_start();
    }

    /// What the run does now. A wait that turns into thinking keeps its start time and its
    /// row; any other change settles what was shown and starts a fresh status.
    pub(super) fn set_phase(&mut self, phase: Phase) {
        if !self.live {
            return;
        }
        enum Change {
            Same,
            Wait,
            Other,
        }
        let change = match &self.status {
            Some(status) if status.phase == phase => Change::Same,
            Some(status) if matches!((&status.phase, &phase), (Phase::Model { .. }, Phase::Model { .. })) => {
                Change::Wait
            }
            Some(_) | None => Change::Other,
        };
        match change {
            Change::Same => {}
            Change::Wait => {
                let drawn = if let Some(status) = &mut self.status {
                    status.phase = phase;
                    status.drawn_chars > 0
                } else {
                    false
                };
                if drawn {
                    self.draw();
                }
            }
            Change::Other => {
                self.settle_status();
                self.status = Some(StatusLine {
                    phase,
                    since: Instant::now(),
                    drawn_chars: 0,
                    frame: 0,
                });
            }
        }
    }

    /// Nothing to show any more: a drawn status stays as a settled line.
    pub(super) fn clear_status(&mut self) {
        self.settle_status();
        self.status = None;
    }

    /// The tool's result line takes the place of the tool's status line.
    pub(super) fn tool_done(&mut self, tone: Tone, text: &str) {
        let in_place = self
            .status
            .as_ref()
            .is_some_and(|status| matches!(status.phase, Phase::Tool { .. }) && status.drawn_chars > 0);
        if in_place {
            let painted = self.style.paint(tone, text);
            let drawn = self.status.as_ref().map_or(0, |status| status.drawn_chars);
            self.rewrite_row(&painted, text.chars().count(), drawn);
            self.write(Stream::Stderr, "\n");
            self.status = None;
        } else {
            self.status = None;
            self.line(tone, text);
        }
    }

    /// Redraw the status, once a second.
    pub(super) fn tick(&mut self) {
        let Some(status) = &mut self.status else {
            return;
        };
        if status.since.elapsed() < STATUS_DELAY {
            return;
        }
        status.frame = (status.frame + 1) % SPINNER.len();
        self.draw();
    }

    fn draw(&mut self) {
        let Some(status) = &self.status else {
            return;
        };
        let elapsed = status.since.elapsed().as_secs();
        let text = match &status.phase {
            Phase::Model { thinking: false } => format!("{} waiting for the model · {elapsed}s", SPINNER[status.frame]),
            Phase::Model { thinking: true } => format!("{} thinking · {elapsed}s", SPINNER[status.frame]),
            Phase::Tool { label } => format!("{} {label} · {elapsed}s", SPINNER[status.frame]),
        };
        let text = clip_chars(&text, status_max_chars());
        let chars = text.chars().count();
        let drawn = status.drawn_chars;
        let painted = self.style.paint(Tone::Dim, &text);
        if drawn == 0 {
            self.ensure_line_start();
            self.write(Stream::Stderr, &painted);
        } else {
            self.rewrite_row(&painted, chars, drawn);
        }
        if let Some(status) = &mut self.status {
            status.drawn_chars = chars.max(drawn);
        }
    }

    /// A drawn status becomes a plain line that says what happened and how long it took.
    fn settle_status(&mut self) {
        let Some(status) = &self.status else {
            return;
        };
        if status.drawn_chars == 0 {
            return;
        }
        let elapsed = elapsed_text(status.since.elapsed());
        let text = match &status.phase {
            Phase::Model { thinking: false } => format!("· waited {elapsed} for the model"),
            Phase::Model { thinking: true } => format!("· thought for {elapsed}"),
            Phase::Tool { label } => format!("· {label} · {elapsed}"),
        };
        let text = clip_chars(&text, status_max_chars());
        let painted = self.style.paint(Tone::Dim, &text);
        let drawn = status.drawn_chars;
        self.rewrite_row(&painted, text.chars().count(), drawn);
        self.write(Stream::Stderr, "\n");
        if let Some(status) = &mut self.status {
            status.drawn_chars = 0;
        }
    }

    /// Overwrite the current row from its start. Leftover characters of a wider earlier text
    /// are blanked; nothing to the right of that is erased, so what a user has typed survives.
    fn rewrite_row(&mut self, painted: &str, chars: usize, drawn_chars: usize) {
        let pad = " ".repeat(drawn_chars.saturating_sub(chars));
        if pad.is_empty() {
            self.write(Stream::Stderr, &format!("\r{painted}"));
        } else {
            self.write(Stream::Stderr, &format!("\r{painted}{pad}\r{painted}"));
        }
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

/// One less than the terminal's width, so that the cursor never wraps to the next row.
fn status_max_chars() -> usize {
    terminal_columns().map_or(STATUS_FALLBACK_CHARS, |columns| {
        columns.saturating_sub(1).max(STATUS_MIN_CHARS)
    })
}

/// The width of the terminal on stderr; `None` when it is not one or cannot say.
#[cfg(unix)]
fn terminal_columns() -> Option<usize> {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ fills one `winsize`, which `size` provides for the duration of the
    // call; stderr is a descriptor of this process, and a failure only returns -1.
    let result = unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut size) };
    (result == 0 && size.ws_col > 0).then(|| usize::from(size.ws_col))
}

#[cfg(not(unix))]
fn terminal_columns() -> Option<usize> {
    None
}

fn clip_chars(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((index, _)) => text[..index].to_string(),
        None => text.to_string(),
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
    fn the_status_width_falls_back_without_a_terminal_and_never_drops_below_the_minimum() {
        // The test's stderr is not a terminal, so the fallback applies.
        assert_eq!(status_max_chars(), STATUS_FALLBACK_CHARS);
    }

    #[test]
    fn elapsed_shows_tenths_only_under_ten_seconds() {
        assert_eq!(elapsed_text(Duration::from_millis(1234)), "1.2s");
        assert_eq!(elapsed_text(Duration::from_millis(12_345)), "12s");
    }

    #[test]
    fn a_phase_change_within_a_wait_keeps_its_start_time() {
        let mut terminal = Terminal::new(Style { color: false }, true, false);
        terminal.set_phase(Phase::Model { thinking: false });
        let since = terminal.status.as_ref().unwrap().since;
        terminal.set_phase(Phase::Model { thinking: true });
        assert_eq!(terminal.status.as_ref().unwrap().since, since);
        terminal.set_phase(Phase::Tool {
            label: "bash: ls".into(),
        });
        assert!(terminal.status.as_ref().unwrap().since >= since);
        terminal.clear_status();
        assert!(terminal.status.is_none());
    }

    #[test]
    fn statuses_are_ignored_without_an_interactive_stderr() {
        let mut terminal = Terminal::new(Style { color: false }, false, false);
        terminal.set_phase(Phase::Model { thinking: false });
        assert!(terminal.status.is_none());
    }

    #[test]
    fn clipping_keeps_whole_characters() {
        assert_eq!(clip_chars("你好世界", 2), "你好");
        assert_eq!(clip_chars("ab", 5), "ab");
    }
}
