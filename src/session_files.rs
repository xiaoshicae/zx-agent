//! Session files: where every mode opens or creates one, and where chat and acp keep and
//! list them.
//!
//! As in pi (`getDefaultSessionDirPath`), each working directory has its own directory under
//! the zx-agent home directory, which `ZX_AGENT_HOME` moves.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use zx_agent::config;
use zx_agent::session::{self, SessionHeader, SessionInfo, SessionList, SessionStore};

use crate::render::{local_time, visible};

/// Under the zx-agent home directory: the sessions of every working directory.
const SESSIONS_DIR: &str = "sessions";
/// Characters of a first prompt shown in a listing.
const PROMPT_PREVIEW_CHARS: usize = 60;
/// Characters of a session ID shown in a listing; enough to tell sessions apart, and what
/// `--resume` accepts as a prefix.
const SHORT_ID_CHARS: usize = 8;
/// How a listing shows when a session was last active.
const LISTING_TIME_FORMAT: &str = "%Y-%m-%d %H:%M";

/// Where a mode keeps its session.
#[derive(Clone, Copy)]
pub(crate) enum SessionFile<'a> {
    Memory,
    Open(&'a Path),
    Create(&'a Path),
    /// A new file in this directory, named after the session ([`file_name`]); the directory
    /// is created when missing.
    NewIn(&'a Path),
}

impl<'a> SessionFile<'a> {
    pub(crate) fn from_path(path: Option<&'a Path>) -> Self {
        match path {
            Some(path) => Self::Open(path),
            None => Self::Memory,
        }
    }

    /// The session file, or for [`SessionFile::NewIn`] the directory it will be created in.
    pub(crate) fn location(&self) -> Option<&'a Path> {
        match self {
            Self::Memory => None,
            Self::Open(path) | Self::Create(path) | Self::NewIn(path) => Some(path),
        }
    }
}

/// The session `file` names when it exists already; opened before anything else is started,
/// for its ID.
pub(crate) fn open_existing(file: &SessionFile<'_>) -> Result<Option<SessionStore>> {
    Ok(match file {
        SessionFile::Open(path) if path.exists() => Some(SessionStore::open(path)?),
        SessionFile::Memory | SessionFile::Open(_) | SessionFile::Create(_) | SessionFile::NewIn(_) => None,
    })
}

/// A new session with `header`, kept where `file` says.
pub(crate) fn create(file: SessionFile<'_>, header: SessionHeader) -> Result<SessionStore> {
    Ok(match file {
        SessionFile::Memory => SessionStore::in_memory(header),
        SessionFile::Open(path) | SessionFile::Create(path) => SessionStore::create(path, header)?,
        SessionFile::NewIn(dir) => {
            create_dir(dir)?;
            SessionStore::create(&dir.join(file_name(&header)), header)?
        }
    })
}

/// `sessions/--<cwd>--` in the zx-agent home directory (`ZX_AGENT_HOME`, else `~/.zx-agent`).
pub(crate) fn default_dir(cwd: &Path) -> Result<PathBuf> {
    let home = config::home()?.context(
        "cannot find the home directory for session files; set ZX_AGENT_HOME, or give --session or --no-session",
    )?;
    Ok(dir_in(&home, cwd))
}

/// `sessions/--<cwd>--` in `home`, with the separators of `cwd` turned into `-` as pi does, so
/// one directory holds one working directory's sessions.
fn dir_in(home: &Path, cwd: &Path) -> PathBuf {
    let path = cwd.display().to_string();
    let encoded: String = path
        .strip_prefix(['/', '\\'])
        .unwrap_or(&path)
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':') { '-' } else { c })
        .collect();
    home.join(SESSIONS_DIR).join(format!("--{encoded}--"))
}

/// Create `dir` and its missing parents, readable only by this user on Unix: session files
/// hold the conversation and tool output. Existing directories keep their permissions.
fn create_dir(dir: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(dir)
        .with_context(|| format!("cannot create the session directory {}", dir.display()))
}

/// `<UTC time>_<session ID>.jsonl`, as pi names them: sorted by name they are in the order
/// they were started, and the ID in the name is the one `--resume` takes.
fn file_name(header: &SessionHeader) -> String {
    let time = i64::try_from(header.timestamp)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        // A clock beyond the year 262143 cannot occur; the ID alone still names the file uniquely.
        .map(|utc| utc.format("%Y-%m-%dT%H-%M-%S-%3fZ_").to_string())
        .unwrap_or_default();
    format!("{time}{}.jsonl", header.id)
}

/// List the sessions in `dir`, reading files on a blocking thread.
pub(crate) async fn list(dir: &Path) -> Result<SessionList> {
    let read = dir.to_path_buf();
    tokio::task::spawn_blocking(move || session::list_sessions(&read))
        .await
        .with_context(|| format!("listing the sessions in {} stopped", dir.display()))?
}

/// One line per session: short ID, last activity in local time, message count and the
/// session's name, else its first prompt; sessions started elsewhere name their working
/// directory.
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
                short_id(&session.id),
                local_time(session.last_activity_ms, LISTING_TIME_FORMAT).unwrap_or_else(|| "-".to_string()),
                session.messages,
                preview(session)
            )
        })
        .collect()
}

/// The first [`SHORT_ID_CHARS`] of a session ID, as listings show it.
fn short_id(id: &str) -> String {
    id.chars().take(SHORT_ID_CHARS).collect()
}

fn preview(session: &SessionInfo) -> String {
    match session.title() {
        Some(title) => preview_text(title),
        None => "(no prompt)".to_string(),
    }
}

/// The first non-empty line of `text`, at most [`PROMPT_PREVIEW_CHARS`] characters, with an
/// ellipsis for what is left out and control characters made visible.
pub(crate) fn preview_text(text: &str) -> String {
    let line = text.lines().find(|line| !line.trim().is_empty()).unwrap_or("").trim();
    let mut shown: String = line.chars().take(PROMPT_PREVIEW_CHARS).collect();
    if line.chars().count() > PROMPT_PREVIEW_CHARS || text.trim().lines().count() > 1 {
        shown.push('…');
    }
    visible(&shown)
}

#[cfg(test)]
mod tests {
    use zx_agent::session::SessionName;

    use super::*;

    #[test]
    fn the_default_directory_names_the_working_directory_as_pi_does() {
        let dir = dir_in(Path::new("/home/me/.zx-agent"), Path::new("/Users/me/work:repo"));
        assert_eq!(dir, Path::new("/home/me/.zx-agent/sessions/--Users-me-work-repo--"));
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
            name: None,
        };
        assert_eq!(preview(&session(Some("fix the build"))), "fix the build");
        assert_eq!(preview(&session(Some("\nfirst\nsecond"))), "first…");
        assert_eq!(
            preview(&session(Some(&"界".repeat(61)))),
            format!("{}…", "界".repeat(60))
        );
        assert_eq!(preview(&session(None)), "(no prompt)");
        let named = SessionInfo {
            name: SessionName::new(&"名".repeat(61)),
            ..session(Some("fix the build"))
        };
        assert_eq!(preview(&named), format!("{}…", "名".repeat(60)));
    }
}
