//! How a listing shows sessions: one line each, with a short ID and a preview.

use zx_agent::transcript::{SessionInfo, SessionList};

use crate::render::{local_time, visible};

/// Characters of a first prompt shown in a listing.
const PROMPT_PREVIEW_CHARS: usize = 60;
/// Characters of a session ID shown in a listing; enough to tell sessions apart, and what
/// `--resume` accepts as a prefix.
const SHORT_ID_CHARS: usize = 8;
/// How a listing shows when a session was last active.
const LISTING_TIME_FORMAT: &str = "%Y-%m-%d %H:%M";

/// One line per session: short ID, last activity in local time, message count and the
/// session's name, else its first prompt.
pub(crate) fn lines(list: &SessionList) -> Vec<String> {
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
