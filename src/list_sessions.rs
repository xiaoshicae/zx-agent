//! `zx-agent sessions`: the session files chat keeps for a working directory, most recent
//! first.

use anyhow::Result;

use crate::cli::{SessionsArgs, resolve_cwd};
use crate::process::{EXIT_COMPLETED, print_line};
use crate::render::visible;
use crate::sessions;

/// List the sessions on stdout, newest first; the directory and the files that could not be
/// read go to stderr.
pub(crate) async fn run(args: SessionsArgs) -> Result<u8> {
    let cwd = resolve_cwd(args.cwd.as_deref())?;
    let dir = match args.session_dir {
        Some(dir) => dir,
        None => sessions::default_dir(&cwd)?,
    };
    let list = sessions::list(&dir).await?;
    for line in sessions::lines(&list, &cwd) {
        print_line(&line);
    }
    for unreadable in &list.unreadable {
        eprintln!(
            "[sessions] cannot read {}: {}",
            visible(&unreadable.path.display().to_string()),
            visible(&unreadable.error)
        );
    }
    eprintln!(
        "[sessions] {} session(s) in {}",
        list.sessions.len(),
        visible(&dir.display().to_string())
    );
    Ok(EXIT_COMPLETED)
}
