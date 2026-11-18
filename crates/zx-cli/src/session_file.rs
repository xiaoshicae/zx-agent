//! `--session`: a file that holds one session. run and chat continue the session it holds, in
//! the directory that session started in, or start one in a file that does not exist yet.

use std::path::Path;

use anyhow::{Result, bail};

use zx_agent::transcript::{SessionInfo, SessionQuery};
use zx_app::{SessionPlace, resolve_cwd};

/// The ID of the session the file `path` holds already, which then continues; `None` for a
/// file that does not exist yet. The file is kept in [`SessionPlace::file`].
///
/// Fails for a file that cannot be read as a session, and when `cwd`, the directory `--cwd`
/// gave, is another than the one the session works in.
pub(crate) async fn held(path: &Path, cwd: Option<&Path>) -> Result<Option<String>> {
    let Some(held) = only(path).await? else {
        return Ok(None);
    };
    check_session_cwd(cwd, &held.cwd)?;
    Ok(Some(held.id))
}

/// The session the file `path` holds; `None` for a file that does not exist yet. A file that
/// cannot be read as a session is an error.
async fn only(path: &Path) -> Result<Option<SessionInfo>> {
    let list = SessionPlace::file(path)
        .storage()
        .list(&SessionQuery::default())
        .await?;
    if let Some(unreadable) = list.unreadable.first() {
        bail!("{}", unreadable.error);
    }
    Ok(list.sessions.into_iter().next())
}

/// Refuse `--cwd`'s directory, `explicit`, when it is another than `session_cwd`, where the
/// session to continue works: a session goes on in the directory it started in, and `--cwd`
/// is where new ones work. Without `--cwd` nothing is asked.
fn check_session_cwd(explicit: Option<&Path>, session_cwd: &str) -> Result<()> {
    let Some(explicit) = explicit else {
        return Ok(());
    };
    let asked = resolve_cwd(Some(explicit))?;
    // A recorded directory that is gone is not the one asked for either; opening the
    // session says that it is gone.
    let same = Path::new(session_cwd)
        .canonicalize()
        .is_ok_and(|recorded| recorded == asked);
    anyhow::ensure!(
        same,
        "--cwd {} is not where the session works: it started in {session_cwd} and goes on there; leave out --cwd to continue it",
        asked.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use zx_agent::ai::Message;
    use zx_agent::transcript::{SessionHeader, Transcript};

    use super::*;

    #[tokio::test]
    async fn a_file_holds_its_session_once_it_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");
        assert_eq!(only(&path).await.unwrap(), None);
        assert_eq!(held(&path, None).await.unwrap(), None);

        let header = SessionHeader::new(Path::new("/w"), "m");
        let log = SessionPlace::file(&path).storage().create(&header).await.unwrap();
        let mut session = Transcript::with_log(header.clone(), log);
        session.append_message(Message::user("q")).await.unwrap();
        drop(session);

        let only = only(&path).await.unwrap().unwrap();
        assert_eq!(
            (only.id.as_str(), only.cwd.as_str(), only.messages),
            (header.id.as_str(), "/w", 1)
        );
        assert_eq!(held(&path, None).await.unwrap(), Some(header.id));
    }

    #[tokio::test]
    async fn a_file_that_is_not_a_session_is_an_error_that_names_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");
        std::fs::write(&path, "not json\n").unwrap();

        let error = held(&path, None).await.unwrap_err();

        let text = format!("{error:#}");
        assert!(
            text.contains("invalid session header") && text.contains("run.jsonl"),
            "{text}"
        );
    }
}
