//! `zx-agent sessions`: the session files chat keeps for a working directory, most recent
//! first.

use anyhow::Result;

use crate::cli::{SessionsArgs, resolve_cwd};
use crate::process::{EXIT_COMPLETED, print_line};
use crate::render::visible;
use crate::session_files::{self, SessionPlace};

/// List the sessions started in the working directory on stdout, newest first; their
/// directory and the files in it that could not be read go to stderr.
pub(crate) async fn run(args: SessionsArgs) -> Result<u8> {
    let cwd = resolve_cwd(args.cwd.as_deref())?;
    let place = SessionPlace::home()?;
    let list = place.started_in(&cwd).await?;
    for line in session_files::lines(&list) {
        print_line(&line);
    }
    for unreadable in &list.unreadable {
        eprintln!(
            "[sessions] cannot read {}: {}",
            visible(&unreadable.location),
            visible(&unreadable.error)
        );
    }
    let directory = place.location(&cwd).unwrap_or_default();
    eprintln!(
        "[sessions] {} session(s) in {}",
        list.sessions.len(),
        visible(&directory.display().to_string())
    );
    Ok(EXIT_COMPLETED)
}
