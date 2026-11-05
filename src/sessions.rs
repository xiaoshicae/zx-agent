//! Where chat keeps its session files, forks them, and `zx-agent sessions`, which lists them.
//!
//! As in pi (`getDefaultSessionDirPath`), each working directory has its own directory under
//! the home directory; `--session-dir` replaces it.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;

use zx_agent::session::{self, SessionHeader, SessionInfo, SessionList, SessionStore};

use crate::chat::visible;

/// Under the home directory: the sessions of every working directory.
const SESSIONS_HOME: &str = ".zx-agent/sessions";
/// Characters of a first prompt shown in a listing.
const PREVIEW_CHARS: usize = 60;
/// Characters of a session ID shown in a listing; enough to tell sessions apart, and what
/// `--resume` and `/resume` accept as a prefix.
pub(crate) const SHORT_ID_CHARS: usize = 8;

#[derive(Args)]
pub(crate) struct SessionsArgs {
    /// Directory of session files. Defaults to the one chat uses for --cwd:
    /// ~/.zx-agent/sessions/--<cwd with / as ->--.
    #[arg(long)]
    pub(crate) session_dir: Option<PathBuf>,

    /// Working directory whose default session directory is listed.
    #[arg(long)]
    pub(crate) cwd: Option<PathBuf>,
}

/// The working directory as `prepare` resolves it, which is what session headers record.
pub(crate) fn resolve_cwd(cwd: Option<&Path>) -> Result<PathBuf> {
    let cwd = match cwd {
        Some(path) => path
            .canonicalize()
            .with_context(|| format!("invalid --cwd {}", path.display()))?,
        None => std::env::current_dir()?,
    };
    anyhow::ensure!(cwd.is_dir(), "--cwd is not a directory: {}", cwd.display());
    Ok(cwd)
}

/// `~/.zx-agent/sessions/--<cwd>--`, with the separators of `cwd` turned into `-` as pi does,
/// so one directory holds one working directory's sessions.
pub(crate) fn default_dir(cwd: &Path) -> Result<PathBuf> {
    let home = std::env::home_dir()
        .context("cannot find the home directory for session files; give --session-dir or --no-session")?;
    let path = cwd.display().to_string();
    let encoded: String = path
        .strip_prefix(['/', '\\'])
        .unwrap_or(&path)
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':') { '-' } else { c })
        .collect();
    Ok(home.join(SESSIONS_HOME).join(format!("--{encoded}--")))
}

/// Create `dir` and its missing parents, readable only by this user on Unix: session files
/// hold the conversation and tool output. Existing directories keep their permissions.
pub(crate) fn create_dir(dir: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(dir)
        .with_context(|| format!("cannot create the session directory {}", dir.display()))
}

/// `<UTC time>_<session ID>.jsonl`, as pi names them: sorted by name they are in the order
/// they were started, and the ID in the name is the one `/resume` takes.
pub(crate) fn file_name(header: &SessionHeader) -> String {
    let time = i64::try_from(header.timestamp)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        // A clock beyond the year 262143 cannot occur; the ID alone still names the file uniquely.
        .map(|utc| utc.format("%Y-%m-%dT%H-%M-%S-%3fZ_").to_string())
        .unwrap_or_default();
    format!("{time}{}.jsonl", header.id)
}

/// A session copied by [`fork`].
pub(crate) struct Forked {
    pub(crate) path: PathBuf,
    /// The source session's ID.
    pub(crate) source_id: String,
    /// Entries copied from the source.
    pub(crate) entries: usize,
}

/// Which branch of the source [`fork`] copies.
pub(crate) enum Branch<'a> {
    /// The branch that ends at the source's last record: where the file was last written.
    Last,
    /// The branch that ends at this entry, or nothing for `None`: the position of a session
    /// that is open in this process, which `/branch` moves without writing.
    At(Option<&'a str>),
}

/// Copy a branch of the session file `source` into a new file in `dir`, started in `cwd`
/// with `model` (pi: `createBranchedSession`). The source is read as a snapshot, without its
/// lock, so a session that is open elsewhere, or in this chat, can be forked; it is not
/// changed. A copy that fails part way leaves the new file as far as it got, and the error
/// names it.
pub(crate) async fn fork(source: &Path, dir: &Path, cwd: &Path, model: &str, branch: Branch<'_>) -> Result<Forked> {
    let source = source
        .canonicalize()
        .with_context(|| format!("cannot fork {}", source.display()))?;
    let read = source.clone();
    let source_store = tokio::task::spawn_blocking(move || SessionStore::read(&read))
        .await
        .context("reading the session to fork stopped")??;
    let header = source_store.fork_header(cwd, model);
    create_dir(dir)?;
    let path = dir.join(file_name(&header));
    let mut forked = SessionStore::create(&path, header)?;
    let leaf = match branch {
        Branch::Last => source_store.leaf_id(),
        Branch::At(leaf) => leaf,
    };
    let entries = match leaf {
        Some(leaf) => forked
            .copy_branch(&source_store, leaf)
            .await
            .with_context(|| format!("cannot copy {} into {}", source.display(), path.display()))?,
        None => 0,
    };
    Ok(Forked {
        path,
        source_id: source_store.header().id.clone(),
        entries,
    })
}

/// List the sessions in `dir`, reading files on a blocking thread.
pub(crate) async fn list(dir: PathBuf) -> Result<SessionList> {
    tokio::task::spawn_blocking(move || session::list_sessions(&dir))
        .await
        .context("listing the sessions stopped")?
}

/// One line per session: short ID, last activity in local time, message count and the
/// first prompt; sessions started elsewhere name their working directory.
pub(crate) fn lines(list: &SessionList, cwd: &Path) -> Vec<String> {
    let cwd = cwd.display().to_string();
    list.sessions
        .iter()
        .map(|session| {
            let elsewhere = if session.cwd == cwd {
                String::new()
            } else {
                format!("  (cwd {})", visible(&session.cwd))
            };
            format!(
                "{}  {}  {:>3} message(s)  {}{elsewhere}",
                short_id(session),
                local_time(session.last_activity_ms),
                session.messages,
                preview(session)
            )
        })
        .collect()
}

pub(crate) fn short_id(session: &SessionInfo) -> String {
    session.id.chars().take(SHORT_ID_CHARS).collect()
}

fn local_time(ms: u64) -> String {
    i64::try_from(ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|utc| utc.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
        // Out of range only for a corrupt timestamp, which the listing still shows.
        .unwrap_or_else(|| "-".to_string())
}

fn preview(session: &SessionInfo) -> String {
    match session.first_prompt.as_deref() {
        Some(prompt) => preview_text(prompt),
        None => "(no prompt)".to_string(),
    }
}

/// The first non-empty line of `text`, at most [`PREVIEW_CHARS`] characters, with an
/// ellipsis for what is left out and control characters made visible.
pub(crate) fn preview_text(text: &str) -> String {
    let line = text.lines().find(|line| !line.trim().is_empty()).unwrap_or("").trim();
    let mut shown: String = line.chars().take(PREVIEW_CHARS).collect();
    if line.chars().count() > PREVIEW_CHARS || text.trim().lines().count() > 1 {
        shown.push('…');
    }
    visible(&shown)
}

/// `zx-agent sessions`: sessions on stdout, newest first; the directory and files that could
/// not be read on stderr.
pub(crate) async fn run(args: SessionsArgs) -> Result<u8> {
    let cwd = resolve_cwd(args.cwd.as_deref())?;
    let dir = match args.session_dir {
        Some(dir) => dir,
        None => default_dir(&cwd)?,
    };
    let list = list(dir.clone()).await?;
    let mut stdout = std::io::stdout().lock();
    for line in lines(&list, &cwd) {
        writeln!(stdout, "{line}")?;
    }
    stdout.flush()?;
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
    Ok(crate::EXIT_COMPLETED)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_directory_names_the_working_directory_as_pi_does() {
        let dir = default_dir(Path::new("/Users/me/work:repo")).unwrap();
        assert!(
            dir.ends_with(".zx-agent/sessions/--Users-me-work-repo--"),
            "{}",
            dir.display()
        );
    }

    #[test]
    fn file_names_start_with_the_utc_time_and_end_with_the_session_id() {
        let header = SessionHeader {
            timestamp: 1_762_300_000_123,
            id: "0b7c".into(),
            ..SessionHeader::new(Path::new("/w"), "m")
        };
        assert_eq!(file_name(&header), "2025-11-04T23-46-40-123Z_0b7c.jsonl");
    }

    #[test]
    fn a_preview_shows_the_first_line_and_marks_what_it_leaves_out() {
        let session = |prompt: Option<&str>| SessionInfo {
            path: PathBuf::new(),
            id: String::new(),
            cwd: String::new(),
            started_ms: 0,
            last_activity_ms: 0,
            messages: 0,
            first_prompt: prompt.map(str::to_string),
        };
        assert_eq!(preview(&session(Some("fix the build"))), "fix the build");
        assert_eq!(preview(&session(Some("\nfirst\nsecond"))), "first…");
        assert_eq!(
            preview(&session(Some(&"界".repeat(61)))),
            format!("{}…", "界".repeat(60))
        );
        assert_eq!(preview(&session(None)), "(no prompt)");
    }
}
