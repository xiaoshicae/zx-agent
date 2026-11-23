//! Where the program keeps what outlasts a process, as `[sessions]` says: the sessions, the
//! statuses of runs in progress, the backlog, and in Postgres what each day used. Files in a
//! directory serve one process; Postgres, the replicas of a deployment, one of which leads
//! and serves while the others wait (D44).

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;
use zx_agent::builtin::files::create_private_dir;
use zx_app::SessionPlace;
#[cfg(feature = "postgres")]
use zx_util::stdio::eprint_line;

use super::config_file::SessionsIn;
use crate::backlog::BacklogFiles;
#[cfg(feature = "postgres")]
use crate::postgres::{self, Database, Pace, PostgresBacklog, PostgresSessions, PostgresStatuses, PostgresUsage};
use crate::server::StatusFiles;
use crate::{Backlog, StatusStore, UsageStore};

/// What loses this process the lead, once it is lost, with why: never, for files.
pub(super) type LeadLost = Pin<Box<dyn Future<Output = String> + Send>>;

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
    pub(super) leadership: Leadership,
}

/// When this process may serve: at once, for files, which one process uses; with Postgres,
/// once it leads.
pub(super) struct Leadership {
    #[cfg(feature = "postgres")]
    database: Option<Arc<Database>>,
}

impl Leadership {
    /// Wait for this process's turn to serve, saying on stderr when it waits and when it
    /// leads: what loses it the lead, or `None` when `cancel` comes first. Fails when the
    /// database cannot be reached.
    #[cfg(feature = "postgres")]
    pub(super) async fn take(&self, cancel: &CancellationToken) -> Result<Option<LeadLost>> {
        let Some(database) = &self.database else {
            return Ok(Some(never_lost()));
        };
        let waiting = || eprint_line("[im] another replica leads; this one waits to take over");
        let Some(mut lead) = postgres::lead(database, Pace::default(), cancel, waiting)
            .await
            .map_err(anyhow::Error::msg)?
        else {
            return Ok(None);
        };
        eprint_line(&format!("[im] leading, term {}", lead.term()));
        Ok(Some(Box::pin(async move { lead.lost().await })))
    }

    /// Serve at once: files are one process's.
    #[cfg(not(feature = "postgres"))]
    pub(super) async fn take(&self, _cancel: &CancellationToken) -> Result<Option<LeadLost>> {
        Ok(Some(never_lost()))
    }
}

/// The lead of a process that keeps files, never lost.
fn never_lost() -> LeadLost {
    Box::pin(std::future::pending())
}

/// Open the stores where `sessions` says. Fails when they cannot be made or reached.
pub(super) async fn open(sessions: &SessionsIn) -> Result<Stores> {
    match sessions {
        SessionsIn::Dir(dir) => in_directory(dir),
        SessionsIn::Postgres { url } => in_postgres(url).await,
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
        leadership: Leadership {
            #[cfg(feature = "postgres")]
            database: None,
        },
    })
}

/// The stores in the Postgres at the connection string `url`, their tables made when they are
/// missing.
#[cfg(feature = "postgres")]
async fn in_postgres(url: &str) -> Result<Stores> {
    let database = Database::open(connection(url)?).await.map_err(anyhow::Error::msg)?;
    Ok(Stores {
        place: SessionPlace::from_storage(Arc::new(PostgresSessions::new(Arc::clone(&database)))),
        statuses: Arc::new(PostgresStatuses::new(Arc::clone(&database))),
        backlog: Arc::new(PostgresBacklog::new(Arc::clone(&database))),
        usage: Some(Arc::new(PostgresUsage::new(Arc::clone(&database)))),
        shown: "Postgres".into(),
        leadership: Leadership {
            database: Some(database),
        },
    })
}

/// The connection `url` describes. The error says what is wrong, never the value, which can
/// hold a password: the client names the option that is invalid, or at most one character it
/// did not expect.
#[cfg(feature = "postgres")]
fn connection(url: &str) -> Result<tokio_postgres::Config> {
    url.parse()
        .context("sessions.postgresUrl holds no connection string of Postgres")
}

#[cfg(not(feature = "postgres"))]
async fn in_postgres(_url: &str) -> Result<Stores> {
    anyhow::bail!("sessions.postgresUrl: this build of zx-im has no Postgres; build it with the feature `postgres`")
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
        assert!(stores.usage.is_none());
        for path in [&sessions, &sessions.join(RUNNING_DIR), &sessions.join(BACKLOG_DIR)] {
            let mode = std::fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{}", path.display());
        }
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn a_connection_string_that_is_not_one_is_refused_without_its_value() {
        let url = "postgres://zx:secret@db.internal:port/zx";
        let error = format!("{:#}", connection(url).unwrap_err());
        assert_eq!(
            error,
            "sessions.postgresUrl holds no connection string of Postgres: invalid connection string: invalid value for option `port`"
        );
        assert!(connection("postgres://zx:secret@db.internal:5432/zx").is_ok());
    }

    #[cfg(not(feature = "postgres"))]
    #[tokio::test]
    async fn a_build_without_postgres_refuses_it_by_name() {
        let error = open(&SessionsIn::Postgres {
            url: "postgres://db".into(),
        })
        .await
        .err()
        .unwrap();
        assert!(error.to_string().contains("feature `postgres`"), "{error}");
    }
}
