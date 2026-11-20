//! What only the program's tests can have, which every one of them uses; the rest of what
//! they share is in the `zx-testing` crate, `zx_testing::program` for running a program.
//! Cargo gives the path of the compiled `zx-agent` to the tests of its own package alone.
//! The pseudo-terminal, which needs libc and only two of them use, is in `pty.rs`.

use std::path::Path;

use zx_testing::program::{self, Home};

/// The compiled `zx-agent`, started in `cwd` with the zx-agent home `home` and apart from the
/// developer's environment, as [`program::command`] starts a program.
pub(crate) fn zx_agent_in(cwd: &Path, home: Home<'_>) -> tokio::process::Command {
    program::command(Path::new(env!("CARGO_BIN_EXE_zx-agent")), cwd, home)
}
