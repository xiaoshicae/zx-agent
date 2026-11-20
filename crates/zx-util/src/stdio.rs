//! Lines on stdout and stderr that, unlike `println!` and `eprintln!`, do not panic when the
//! stream is closed.

use std::io::Write;

/// Print `text` and a newline on stdout, unlike `println!` without panicking when stdout is
/// closed. A synchronous write, also from async code: one line, to a terminal, a pipe or a
/// file.
pub fn print_line(text: &str) {
    let mut out = std::io::stdout().lock();
    // A closed stdout (for example a consumer or a log collector that exited) leaves nobody
    // to report to; the program goes on, and its exit code still says how it ended.
    let _ = writeln!(out, "{text}").and_then(|()| out.flush());
}

/// Print `text` and a newline on stderr, unlike `eprintln!` without panicking when stderr is
/// closed. A synchronous write, as [`print_line`]'s.
pub fn eprint_line(text: &str) {
    // A closed stderr leaves nobody to tell; the program goes on, and its exit code says how
    // it ended.
    let _ = writeln!(std::io::stderr().lock(), "{text}");
}
