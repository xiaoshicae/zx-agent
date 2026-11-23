//! The tables, made as the program starts: those of the version it knows, once, while it
//! holds a lock that other replicas starting at the same time wait for.

use tokio_postgres::Client;

use super::Failure;

/// The version of the tables this program makes and reads.
const VERSION: i32 = 1;
/// The advisory lock a start holds while it makes the tables: "zx-im" and 1, a key no other
/// program is likely to take.
const MAKING: i64 = 0x7a78_2d69_6d01;

/// The tables of version 1. Texts of JSON are kept as the program wrote them; times are
/// milliseconds since the Unix epoch.
const TABLES: &str = "
CREATE TABLE zx_im_sessions (
    id TEXT PRIMARY KEY,
    -- The header, as JSON.
    header TEXT NOT NULL,
    -- The header's, for the listings that ask for them.
    cwd TEXT NOT NULL,
    labels JSONB NOT NULL
);
CREATE TABLE zx_im_records (
    session_id TEXT NOT NULL REFERENCES zx_im_sessions (id),
    seq BIGINT NOT NULL,
    -- The record, as JSON: a line of a session file.
    record TEXT NOT NULL,
    -- One record per seq: a second writer's append fails.
    PRIMARY KEY (session_id, seq)
);
CREATE TABLE zx_im_backlog (
    platform TEXT NOT NULL,
    conversation TEXT NOT NULL,
    message TEXT NOT NULL,
    -- What the server keeps of the message; emptied once it is handled.
    record TEXT NOT NULL,
    -- NULL while the message is pending.
    completed_ms BIGINT,
    kept_order BIGSERIAL,
    PRIMARY KEY (platform, conversation, message)
);
CREATE INDEX zx_im_backlog_completed ON zx_im_backlog (completed_ms);
CREATE TABLE zx_im_statuses (
    platform TEXT NOT NULL,
    -- The status message: the chat it was posted in, its thread there, NULL for none, and
    -- its ID.
    status_conversation TEXT NOT NULL,
    status_thread TEXT,
    status_id TEXT NOT NULL,
    -- The conversation of the run, its thread NULL for a direct chat.
    conversation TEXT NOT NULL,
    thread TEXT,
    kept_order BIGSERIAL,
    PRIMARY KEY (platform, status_conversation, status_id)
);
CREATE TABLE zx_im_usage (
    -- Days since 1970-01-01, UTC.
    day BIGINT NOT NULL,
    platform TEXT NOT NULL,
    chat TEXT NOT NULL,
    tokens BIGINT NOT NULL,
    cost DOUBLE PRECISION NOT NULL,
    PRIMARY KEY (day, platform, chat)
);
";

/// Make the tables that are missing, holding [`MAKING`] meanwhile, which waits for another
/// replica that holds it. Fails when the database has tables of another version.
pub(super) async fn make(client: &Client) -> Result<(), Failure> {
    client.execute("SELECT pg_advisory_lock($1)", &[&MAKING]).await?;
    let made = make_while_locked(client).await;
    // The lock goes with the connection anyway; let go of it before then.
    let unlocked = client.execute("SELECT pg_advisory_unlock($1)", &[&MAKING]).await;
    made?;
    Ok(unlocked.map(drop)?)
}

/// The tables of [`VERSION`]: made when there are none, kept when they are of it.
async fn make_while_locked(client: &Client) -> Result<(), Failure> {
    client
        .batch_execute("CREATE TABLE IF NOT EXISTS zx_im_schema (version INT NOT NULL)")
        .await?;
    let rows = client.query("SELECT version FROM zx_im_schema", &[]).await?;
    let version: Option<i32> = match rows.first() {
        Some(row) => Some(row.try_get(0)?),
        None => None,
    };
    match version {
        Some(VERSION) => Ok(()),
        Some(other) => Err(Failure::Other(format!(
            "they are of version {other}, and this zx-im knows version {VERSION}: use the zx-im that made them, or another database"
        ))),
        None => {
            // One transaction: either every table is made, with its version, or none is.
            let tables = format!("BEGIN; {TABLES} INSERT INTO zx_im_schema (version) VALUES ({VERSION}); COMMIT;");
            Ok(client.batch_execute(&tables).await?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::database;

    #[tokio::test]
    async fn the_tables_are_made_once_and_another_version_is_refused() {
        let Some(database) = database().await else {
            return;
        };
        let client = database.client().await.unwrap();
        // Made already by the database's opening: making them again keeps them.
        super::make(&client).await.unwrap();
        let version = client.query("SELECT version FROM zx_im_schema", &[]).await.unwrap();
        assert_eq!(version.len(), 1);
        client
            .execute("UPDATE zx_im_schema SET version = 99", &[])
            .await
            .unwrap();
        let error = super::make(&client).await.unwrap_err().to_string();
        assert!(
            error.starts_with("they are of version 99, and this zx-im knows version 1"),
            "{error}"
        );
    }
}
