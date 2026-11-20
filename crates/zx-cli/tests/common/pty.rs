//! A pseudo-terminal, for the tests of `chat` and `run` that give the program a terminal.
//! Declared only by them (`#[path = "common/pty.rs"]`), so that no other test binary
//! compiles it unused.

use std::os::fd::{FromRawFd, OwnedFd};

/// A new pseudo-terminal: the master end for the test, and the slave end to give a child as
/// its terminal (stdin or stdout).
pub(crate) fn open() -> (tokio::fs::File, OwnedFd) {
    let mut master = -1;
    let mut slave = -1;
    // SAFETY: both output pointers refer to initialized local integers, and null optional
    // name/termios/winsize pointers request the OS defaults. Success yields two fresh FDs.
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
    // SAFETY: openpty succeeded; each descriptor has exactly one owner from here on.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    (tokio::fs::File::from_std(std::fs::File::from(master)), slave)
}
