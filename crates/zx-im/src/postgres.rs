//! Postgres, where the program keeps what its replicas share (D44): the sessions, the backlog,
//! the statuses of runs and what the work of each day used, in tables it makes as it starts.
//! Every store shares one connection, opened again when it was lost; its statements are
//! pipelined on it.
//!
//! - `schema`: the tables, made at the version this program knows
//! - `sessions`: the sessions of the conversations ([`SessionStorage`])
//! - `backlog`: each message until it is handled ([`Backlog`])
//! - `statuses`: the statuses of runs in progress ([`StatusStore`])
//! - `usage`: what the work of each day used ([`UsageStore`])
//! - `test_support`, compiled for tests only: a database for each test, in a schema of its
//!   own
//!
//! [`SessionStorage`]: zx_agent::transcript::SessionStorage
//! [`Backlog`]: crate::Backlog
//! [`StatusStore`]: crate::StatusStore
//! [`UsageStore`]: crate::UsageStore

mod backlog;
mod schema;
mod sessions;
mod statuses;
#[cfg(test)]
mod test_support;
mod usage;

use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use rustls::ClientConfig;
use rustls_platform_verifier::BuilderVerifierExt;
use tokio::sync::Mutex;
use tokio_postgres::error::SqlState;
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, Config, Row};
use tokio_postgres_rustls::MakeRustlsConnect;
use zx_agent::ai::now_ms;

pub(crate) use self::backlog::PostgresBacklog;
pub(crate) use self::sessions::PostgresSessions;
pub(crate) use self::statuses::PostgresStatuses;
pub(crate) use self::usage::PostgresUsage;

/// How long connecting may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long one statement may take, connecting included: a database that answers slower is
/// taken to have failed, rather than holding up the conversation.
const STATEMENT_TIMEOUT: Duration = Duration::from_secs(30);

/// The database, with the one connection every store shares.
pub(crate) struct Database {
    config: Config,
    tls: MakeRustlsConnect,
    /// The connection's client, until the connection is lost: the next statement then
    /// connects again.
    client: Mutex<Option<Arc<Client>>>,
}

/// Why a statement failed.
#[derive(Debug)]
pub(crate) enum Failure {
    /// A row with the same key exists: the statement would have overwritten it.
    Duplicate,
    /// Anything else, as the operator reads it.
    Other(String),
}

impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Duplicate => formatter.write_str("a row with the same key exists"),
            Self::Other(error) => formatter.write_str(error),
        }
    }
}

impl From<tokio_postgres::Error> for Failure {
    fn from(error: tokio_postgres::Error) -> Self {
        if error.code() == Some(&SqlState::UNIQUE_VIOLATION) {
            return Self::Duplicate;
        }
        match error.as_db_error() {
            // The server's own words; the client's error says only that it was the server's.
            Some(db) => Self::Other(format!("{} ({})", db.message(), db.code().code())),
            // What the client was doing, and its sources: why that failed.
            None => Self::Other(format!("{:#}", anyhow::Error::new(error))),
        }
    }
}

impl Database {
    /// Connect as `config` says and make the tables that are missing. TLS follows its
    /// `sslmode`, with the system's trust store.
    ///
    /// Fails when the database cannot be reached or refuses the credentials, and when its
    /// tables are of a version this program does not know.
    pub(crate) async fn open(mut config: Config) -> Result<Arc<Self>, String> {
        config.connect_timeout(CONNECT_TIMEOUT);
        let database = Arc::new(Self {
            config,
            tls: tls()?,
            client: Mutex::new(None),
        });
        let client = timed(database.client())
            .await
            .map_err(|failure| format!("cannot connect to Postgres: {failure}"))?;
        timed(schema::make(&client))
            .await
            .map_err(|failure| format!("cannot set up the tables in Postgres: {failure}"))?;
        Ok(database)
    }

    /// Run `statement` with `params`: the number of rows it changed.
    pub(crate) async fn execute(&self, statement: &str, params: &[&(dyn ToSql + Sync)]) -> Result<u64, Failure> {
        timed(async {
            let client = self.client().await?;
            Ok(client.execute(statement, params).await?)
        })
        .await
    }

    /// Run `statement` with `params`: the rows it returns.
    pub(crate) async fn query(&self, statement: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Vec<Row>, Failure> {
        timed(async {
            let client = self.client().await?;
            Ok(client.query(statement, params).await?)
        })
        .await
    }

    /// The client of an open connection: the one there is, or a new one when the last was
    /// lost.
    async fn client(&self) -> Result<Arc<Client>, Failure> {
        let mut held = self.client.lock().await;
        if let Some(client) = held.as_ref().filter(|client| !client.is_closed()) {
            return Ok(Arc::clone(client));
        }
        let (client, connection) = self.config.connect(self.tls.clone()).await?;
        // It carries the client's statements until the client is dropped or the server goes;
        // then the client is closed, and the next statement connects again. What ended it is
        // what that statement reports.
        tokio::spawn(connection);
        let client = Arc::new(client);
        *held = Some(Arc::clone(&client));
        Ok(client)
    }
}

/// TLS with the system's trust store, as the program's HTTPS has, on aws-lc-rs: the one
/// crypto provider of the build (rule 07), named rather than left to the process's default.
fn tls() -> Result<MakeRustlsConnect, String> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .and_then(BuilderVerifierExt::with_platform_verifier)
        .map_err(|error| format!("cannot set up TLS for Postgres: {error}"))?
        .with_no_client_auth();
    Ok(MakeRustlsConnect::new(config))
}

/// `statement`, within [`STATEMENT_TIMEOUT`].
async fn timed<T>(statement: impl Future<Output = Result<T, Failure>>) -> Result<T, Failure> {
    match tokio::time::timeout(STATEMENT_TIMEOUT, statement).await {
        Ok(result) => result,
        Err(_) => Err(Failure::Other(format!(
            "took longer than {} seconds",
            STATEMENT_TIMEOUT.as_secs()
        ))),
    }
}

/// Now, in milliseconds since the Unix epoch, as the tables keep it.
fn now() -> i64 {
    // Beyond i64 milliseconds, in some 290 million years, it stays at the most.
    i64::try_from(now_ms()).unwrap_or(i64::MAX)
}

/// `value` as a column of `BIGINT`, which stores no more than `i64`.
fn bigint(value: u64, what: &str) -> Result<i64, Failure> {
    i64::try_from(value).map_err(|_| Failure::Other(format!("{what} {value} is beyond what the database keeps")))
}
