//! Where the program keeps what outlasts a process, as `[sessions]` says: the sessions, the
//! statuses of runs in progress, the backlog, and in Postgres what each day used. Files in a
//! directory serve one process; Postgres, the replicas of a deployment.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use zx_agent::builtin::files::create_private_dir;
use zx_app::SessionPlace;
#[cfg(feature = "postgres")]
use zx_util::env::env_value;

use super::config_file::SessionsIn;
use crate::backlog::BacklogFiles;
#[cfg(feature = "postgres")]
use crate::postgres::{Database, PostgresBacklog, PostgresSessions, PostgresStatuses, PostgresUsage};
use crate::server::StatusFiles;
use crate::{Backlog, StatusStore, UsageStore};

/// Under the session directory: the statuses of runs in progress (`StatusFiles`), and the
/// messages received and not handled yet (`BacklogFiles`). The leading dot keeps them apart
/// from the directories of the sessions, whose names start with `--`.
const RUNNING_DIR: &str = ".running";
const BACKLOG_DIR: &str = ".backlog";

/// The stores, open.
pub(super) struct Stores {
    /// Where the agents keep the sessions.
    pub(super) place: SessionPlace,
    pub(super) statuses: Arc<dyn StatusStore>,
    pub(super) backlog: Arc<dyn Backlog>,
    /// What each day used; `None` in a directory, where a restart counts the day from
    /// nothing.
    pub(super) usage: Option<Arc<dyn UsageStore>>,
    /// Where they are, as the startup line says: the directory, or Postgres, without its
    /// connection string, which can hold a password.
    pub(super) shown: String,
    /// The variable that holds the connection string, which commands must not see.
    pub(super) hidden_env: Option<String>,
}

/// Open the stores where `sessions` says. Fails when they cannot be made or reached.
pub(super) async fn open(sessions: &SessionsIn) -> Result<Stores> {
    match sessions {
        SessionsIn::Dir(dir) => in_directory(dir),
        SessionsIn::Postgres { url_env } => in_postgres(url_env).await,
    }
}

/// The stores in the session directory `dir`, created readable only by this user, as the
/// session files are. A startup write; nothing runs concurrently yet.
fn in_directory(dir: &Path) -> Result<Stores> {
    create_private_dir(dir).with_context(|| format!("cannot create the session directory {}", dir.display()))?;
    let running = dir.join(RUNNING_DIR);
    let statuses = StatusFiles::open(&running).with_context(|| format!("cannot create {}", running.display()))?;
    let backlog_dir = dir.join(BACKLOG_DIR);
    let backlog = BacklogFiles::open(&backlog_dir)
        .with_context(|| format!("cannot create the backlog {}", backlog_dir.display()))?;
    Ok(Stores {
        place: SessionPlace::directory(dir),
        statuses: Arc::new(statuses),
        backlog: Arc::new(backlog),
        usage: None,
        shown: dir.display().to_string(),
        hidden_env: None,
    })
}

/// The stores in the Postgres whose connection string the variable `url_env` holds, its
/// tables made when they are missing.
#[cfg(feature = "postgres")]
async fn in_postgres(url_env: &str) -> Result<Stores> {
    let url = env_value(url_env)
        .with_context(|| format!("{url_env} must hold the connection string of Postgres (sessions.postgresUrlEnv)"))?;
    let database = Database::open(connection(url_env, &url)?)
        .await
        .map_err(anyhow::Error::msg)?;
    Ok(Stores {
        place: SessionPlace::from_storage(Arc::new(PostgresSessions::new(Arc::clone(&database)))),
        statuses: Arc::new(PostgresStatuses::new(Arc::clone(&database))),
        backlog: Arc::new(PostgresBacklog::new(Arc::clone(&database))),
        usage: Some(Arc::new(PostgresUsage::new(database))),
        shown: "Postgres".into(),
        hidden_env: Some(url_env.to_string()),
    })
}

/// The connection `url` describes, the value of the variable `url_env`. The error names the
/// variable and what is wrong, never the value, which can hold a password: the client says
/// which option is invalid, or at most one character it did not expect.
#[cfg(feature = "postgres")]
fn connection(url_env: &str, url: &str) -> Result<tokio_postgres::Config> {
    url.parse()
        .with_context(|| format!("{url_env} holds no connection string of Postgres"))
}

#[cfg(not(feature = "postgres"))]
async fn in_postgres(_url_env: &str) -> Result<Stores> {
    anyhow::bail!("sessions.postgresUrlEnv: this build of zx-im has no Postgres; build it with the feature `postgres`")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn only_the_owner_may_list_the_session_directories() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let stores = open(&SessionsIn::Dir(sessions.clone())).await.unwrap();
        assert_eq!(stores.shown, sessions.display().to_string());
        assert!(stores.usage.is_none() && stores.hidden_env.is_none());
        for path in [&sessions, &sessions.join(RUNNING_DIR), &sessions.join(BACKLOG_DIR)] {
            let mode = std::fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{}", path.display());
        }
    }

    #[cfg(feature = "postgres")]
    #[tokio::test]
    async fn a_variable_without_a_connection_string_is_named() {
        // A variable no test or environment sets.
        let unset = SessionsIn::Postgres {
            url_env: "ZX_IM_TEST_POSTGRES_URL_UNSET".into(),
        };
        let error = format!("{:#}", open(&unset).await.err().unwrap());
        assert!(error.starts_with("ZX_IM_TEST_POSTGRES_URL_UNSET must hold"), "{error}");
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn a_connection_string_that_is_not_one_is_refused_without_its_value() {
        let url = "postgres://zx:secret@db.internal:port/zx";
        let error = format!("{:#}", connection("URL", url).unwrap_err());
        assert_eq!(
            error,
            "URL holds no connection string of Postgres: invalid connection string: invalid value for option `port`"
        );
        assert!(connection("URL", "postgres://zx:secret@db.internal:5432/zx").is_ok());
    }

    #[cfg(not(feature = "postgres"))]
    #[tokio::test]
    async fn a_build_without_postgres_refuses_it_by_name() {
        let error = open(&SessionsIn::Postgres { url_env: "URL".into() })
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("feature `postgres`"), "{error}");
    }
}
