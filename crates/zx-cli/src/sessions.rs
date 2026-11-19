//! `zx-agent sessions`: the sessions kept for a working directory, chat's and acp's, one
//! line each, most recent first.

use anyhow::Result;

use zx_agent::transcript::{SessionInfo, SessionList};
use zx_app::{SessionPlace, resolve_cwd};

use crate::cli::SessionsArgs;
use crate::process::{EXIT_COMPLETED, eprint_line, print_line};
use crate::render::{local_time, preview_text, visible};

/// Characters of a session ID shown in a listing; enough to tell sessions apart, and what
/// `--resume` accepts as a prefix.
const SHORT_ID_CHARS: usize = 8;
/// How a listing shows when a session was last active.
const LISTING_TIME_FORMAT: &str = "%Y-%m-%d %H:%M";

/// List the sessions started in the working directory on stdout, newest first; their
/// directory and the files in it that could not be read go to stderr.
pub(crate) async fn run(args: SessionsArgs) -> Result<u8> {
    let cwd = resolve_cwd(args.cwd.as_deref())?;
    let place = SessionPlace::home()?;
    let list = place.started_in(&cwd).await?;
    for line in lines(&list) {
        print_line(&line);
    }
    for unreadable in &list.unreadable {
        eprint_line(&format!("[sessions] {}", visible(&unreadable.error)));
    }
    let directory = place.location(&cwd).unwrap_or_default();
    eprint_line(&format!(
        "[sessions] {} session(s) in {}",
        list.sessions.len(),
        visible(&directory.display().to_string())
    ));
    Ok(EXIT_COMPLETED)
}

/// One line per session: short ID, last activity in local time, message count and the
/// session's name, else its first prompt.
fn lines(list: &SessionList) -> Vec<String> {
    list.sessions
        .iter()
        .map(|session| {
            format!(
                "{}  {}  {:>3} message(s)  {}",
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use zx_agent::transcript::SessionName;

    use super::*;

    #[test]
    fn a_preview_shows_the_first_line_and_marks_what_it_leaves_out() {
        let session = |prompt: Option<&str>| SessionInfo {
            id: String::new(),
            cwd: String::new(),
            started_ms: 0,
            last_activity_ms: 0,
            messages: 0,
            first_prompt: prompt.map(str::to_string),
            name: None,
            labels: BTreeMap::new(),
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
