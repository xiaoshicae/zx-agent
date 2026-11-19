//! The sessions a client opens: created for its project directory, found again by ID among
//! the session files of a directory, replayed when it loads one, and listed.

use std::path::{Path, PathBuf};

use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    ListSessionsRequest, LoadSessionRequest, ResumeSessionRequest, SessionId, SessionInfo,
};
use agent_client_protocol::{Client, Error, ErrorCode};
use anyhow::{Context, Result};

use zx_agent::Session;
use zx_agent::transcript::SessionInfo as StoredSession;
use zx_app::{ConfiguredAgent, resolve_cwd};

use super::updates::Updates;
use crate::application::open_session;
use crate::process::eprint_line;
use crate::render::{preview_text, utc_time, visible};

/// Create a session for `cwd`, the client's project directory, whatever `--cwd` says.
/// It is kept among the session files, where `session/load` finds it again.
pub(super) async fn create(configured: &ConfiguredAgent, cwd: &Path) -> Result<SessionId> {
    let session = configured.create(Some(cwd)).await?;
    Ok(SessionId::new(session.id()))
}

/// The session `id` for `cwd`: the one this process has open, or the one kept among the
/// session files, opened, when it was started in `cwd`.
///
/// A kept session is asked for by its ID: its file must be among the sessions of `cwd`
/// before it is opened, which sets it up. A listing of the directory would read every
/// session in it.
async fn locate(configured: &ConfiguredAgent, id: &SessionId, cwd: &Path) -> Result<Session> {
    let open = configured.agent().session(&id.0);
    let (id, place) = (&*id.0, configured.place());
    let cwd = resolve_cwd(Some(cwd))?;
    let directory = place.location(&cwd);
    let unknown = || {
        let searched = directory.as_deref().unwrap_or(&cwd);
        unknown_session(id, &format!("no such session in {}", searched.display()))
    };
    if let Some(session) = open {
        // Open in this process, it is still only a session of the directory it works in.
        return if session.cwd() == cwd {
            Ok(session)
        } else {
            Err(unknown().into())
        };
    }
    let Some(kept) = directory.as_deref() else {
        return Err(unknown_session(id, "the agent keeps no session files (--no-session)").into());
    };
    let kept_there = place.path_of(id).await?.is_some_and(|path| path.parent() == Some(kept));
    if !kept_there {
        return Err(unknown().into());
    }
    let session = open_session(configured, id).await?;
    // Two directories can share a directory name of sessions (`/a-b` and `/a/b`).
    if session.cwd() != cwd {
        // Started in another directory: not a session of this one, and not left open,
        // where the next request would find it whatever directory it names.
        configured.agent().close_session(id);
        return Err(unknown().into());
    }
    Ok(session)
}

/// Load the session `request` names from the session directory of its `cwd`, replaying
/// the conversation to the client first.
///
/// A session this process has open already is replayed as it is, without opening its
/// file again.
pub(super) async fn load(
    configured: &ConfiguredAgent,
    cx: &ConnectionTo<Client>,
    request: &LoadSessionRequest,
) -> Result<SessionId> {
    let id = &request.session_id;
    let session = locate(configured, id, &request.cwd).await?;
    let updates = Updates::new(cx.clone(), id.clone(), session.cwd().to_path_buf());
    session
        .with_transcript(|transcript| updates.replay(transcript))
        .with_context(|| format!("cannot replay session {}", id.0))?;
    Ok(id.clone())
}

/// Continue the session `request` names without replaying it (`session/resume`): the
/// client keeps what it shows, and the conversation goes on from the file.
pub(super) async fn resume(configured: &ConfiguredAgent, request: &ResumeSessionRequest) -> Result<SessionId> {
    locate(configured, &request.session_id, &request.cwd).await?;
    Ok(request.session_id.clone())
}

/// The sessions kept that were started in `request.cwd`, or in `--cwd`'s directory when
/// the request names none, most recently active first; none with `--no-session`, which
/// keeps none. A listing is one page, so a cursor is never one of ours.
pub(super) async fn list(configured: &ConfiguredAgent, request: &ListSessionsRequest) -> Result<Vec<SessionInfo>> {
    if let Some(cursor) = &request.cursor {
        return Err(Error::new(
            ErrorCode::INVALID_PARAMS,
            format!("unknown cursor {cursor:?}: a listing is one page"),
        )
        .into());
    }
    let cwd = configured.new_session_cwd(request.cwd.as_deref())?;
    if !configured.place().is_persistent() {
        return Ok(Vec::new());
    }
    let kept = configured.place().started_in(&cwd).await?;
    // The client gets what could be read; stderr is where the agent reports the rest.
    for unreadable in &kept.unreadable {
        eprint_line(&format!("[acp] {}", visible(&unreadable.error)));
    }
    Ok(kept.sessions.iter().map(session_info).collect())
}

/// A listed session as the client shows it: its name, else its first prompt, as the title,
/// and its last activity as the time.
fn session_info(session: &StoredSession) -> SessionInfo {
    let updated_at =
        utc_time(session.last_activity_ms).map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
    SessionInfo::new(SessionId::new(session.id.as_str()), PathBuf::from(&session.cwd))
        .title(session.title().map(preview_text))
        .updated_at(updated_at)
}

/// The error for a session the agent does not have, with the reason.
pub(super) fn unknown_session(id: &str, why: &str) -> Error {
    Error::new(ErrorCode::INVALID_PARAMS, format!("unknown session {id}: {why}"))
}
