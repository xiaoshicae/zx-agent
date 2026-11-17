//! What only the program's tests can have; the rest of what they share is in the
//! `zx-testing` crate, `zx_testing::program` for running a program. Cargo gives the path of
//! the compiled `zx-agent` to the tests of its own package alone, and the pseudo-terminal
//! needs libc.

#![allow(
    dead_code,
    reason = "each test binary uses a different subset of these helpers; `expect` would fail in a binary that uses them all"
)]

use std::path::Path;

use zx_testing::program::{self, Home};

/// The compiled `zx-agent`, started in `cwd` with the zx-agent home `home` and apart from the
/// developer's environment, as [`program::command`] starts a program.
pub(crate) fn zx_agent_in(cwd: &Path, home: Home<'_>) -> tokio::process::Command {
    program::command(Path::new(env!("CARGO_BIN_EXE_zx-agent")), cwd, home)
}

/// A pseudo-terminal: the master end for the test, and the slave end to give a child as
/// its terminal (stdin or stdout).
#[cfg(unix)]
pub(crate) fn pty() -> (tokio::fs::File, std::os::fd::OwnedFd) {
    use std::os::fd::{FromRawFd, OwnedFd};
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
