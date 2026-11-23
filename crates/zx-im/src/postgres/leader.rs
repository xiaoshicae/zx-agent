//! Who leads (D44): the replicas of a deployment contend for an advisory lock, each on a
//! connection of its own, which a lost connection releases. The one that holds it leads, in a
//! term it raises as it takes the lead, and that each write of the stores checks; it watches
//! the connection, and its lead is over when the connection is lost.

use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_postgres::{Client, Row};
use tokio_util::sync::CancellationToken;

use super::{Database, Failure, timed};

/// What the server keeps up on the connection that holds the lock, so that the lock is free
/// within about half a minute of the leader's network going: a probe after 10 seconds of
/// silence, then every 5 seconds, 3 unanswered ending the connection.
const KEEPALIVES: &str = "SET tcp_keepalives_idle = 10; SET tcp_keepalives_interval = 5; SET tcp_keepalives_count = 3";

/// How often a replica tries for the lead while another holds it, and how often, and how
/// patiently, the leader makes sure it still leads.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Pace {
    pub(crate) try_every: Duration,
    pub(crate) check_every: Duration,
    pub(crate) check_timeout: Duration,
}

impl Default for Pace {
    /// A standby takes over within 2 seconds of the lock going free; the leader notices a
    /// lost lead within 10.
    fn default() -> Self {
        Self {
            try_every: Duration::from_secs(2),
            check_every: Duration::from_secs(5),
            check_timeout: Duration::from_secs(5),
        }
    }
}

/// The lead of this process, until it is lost or dropped; dropping it lets go of the lock.
pub(crate) struct Lead {
    term: i64,
    /// Holds the lock, and ends with what ended the lead.
    watch: JoinHandle<String>,
}

impl Lead {
    /// The term this process leads in.
    pub(crate) fn term(&self) -> i64 {
        self.term
    }

    /// What ended the lead, once it ends. Called once.
    pub(crate) async fn lost(&mut self) -> String {
        match (&mut self.watch).await {
            Ok(why) => why,
            Err(error) => format!("the watch over the lead stopped: {error}"),
        }
    }
}

impl Drop for Lead {
    fn drop(&mut self) {
        // With the watch go its client and connection, and with them the lock.
        self.watch.abort();
    }
}

/// Wait until this process leads, trying every `pace.try_every`, and call `waiting` once
/// should another replica lead at first; `None` when `cancel` comes first. Fails when the
/// database cannot be reached, or the term cannot be raised.
pub(crate) async fn lead(
    database: &Database,
    pace: Pace,
    cancel: &CancellationToken,
    waiting: impl FnOnce(),
) -> Result<Option<Lead>, String> {
    let failed = |failure: Failure| format!("cannot take the lead: {failure}");
    let (client, connection) = timed(async { Ok(database.config.connect(database.tls.clone()).await?) })
        .await
        .map_err(failed)?;
    // It ends with the client, or when the server goes: the watch reports which.
    let connection = tokio::spawn(connection);
    timed(async { Ok(client.batch_execute(KEEPALIVES).await?) })
        .await
        .map_err(failed)?;
    let key: i64 = asked(&client, "SELECT lock_key FROM zx_im_leader")
        .await
        .and_then(|row| Ok(row.try_get(0)?))
        .map_err(failed)?;
    let mut waiting = Some(waiting);
    loop {
        let held: bool = timed(async { Ok(client.query_one("SELECT pg_try_advisory_lock($1)", &[&key]).await?) })
            .await
            .and_then(|row| Ok(row.try_get(0)?))
            .map_err(failed)?;
        if held {
            break;
        }
        if let Some(waiting) = waiting.take() {
            waiting();
        }
        tokio::select! {
            () = cancel.cancelled() => return Ok(None),
            () = tokio::time::sleep(pace.try_every) => {}
        }
    }
    // Waits for the writes of the term before, which hold its row until they are done.
    let term: i64 = asked(&client, "UPDATE zx_im_leader SET term = term + 1 RETURNING term")
        .await
        .and_then(|row| Ok(row.try_get(0)?))
        .map_err(failed)?;
    database.lead_in(term).map_err(failed)?;
    let watch = tokio::spawn(watch(client, connection, term, pace));
    Ok(Some(Lead { term, watch }))
}

/// The one row `statement` returns on `client`, within the statement timeout.
async fn asked(client: &Client, statement: &str) -> Result<Row, Failure> {
    timed(async { Ok(client.query_one(statement, &[]).await?) }).await
}

/// Watch the lead in `term`, held on `client`, whose connection runs as `connection`: what
/// ended it, once something does. It holds `client`, and so the lock, until then.
async fn watch(
    client: Client,
    mut connection: JoinHandle<Result<(), tokio_postgres::Error>>,
    term: i64,
    pace: Pace,
) -> String {
    loop {
        tokio::select! {
            ended = &mut connection => return match ended {
                Ok(Ok(())) => "the connection that holds the lead closed".into(),
                Ok(Err(error)) => format!("the connection that holds the lead failed: {}", Failure::from(error)),
                Err(error) => format!("the connection that holds the lead stopped: {error}"),
            },
            () = tokio::time::sleep(pace.check_every) => {}
        }
        let checked = tokio::time::timeout(
            pace.check_timeout,
            client.query_one("SELECT term FROM zx_im_leader", &[]),
        )
        .await;
        let now: Result<i64, Failure> = match checked {
            Ok(answer) => answer.and_then(|row| row.try_get(0)).map_err(Failure::from),
            Err(_) => Err(Failure::Other(format!(
                "no answer in {} seconds",
                pace.check_timeout.as_secs()
            ))),
        };
        match now {
            Ok(now) if now == term => {}
            Ok(now) => return format!("another replica leads now, in term {now}"),
            Err(failure) => return format!("cannot make sure this replica still leads: {failure}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::super::test_support::{database, replica};
    use super::*;

    /// A quick pace, for tests.
    const QUICK: Pace = Pace {
        try_every: Duration::from_millis(50),
        check_every: Duration::from_millis(50),
        check_timeout: Duration::from_secs(5),
    };

    /// The lead of `database`, taken at once or not within a second.
    async fn lead_soon(database: &Database) -> Option<Lead> {
        let cancel = CancellationToken::new();
        let taking = lead(database, QUICK, &cancel, || {});
        tokio::time::timeout(Duration::from_secs(1), taking)
            .await
            .ok()
            .map(|lead| lead.unwrap().unwrap())
    }

    #[tokio::test]
    async fn one_replica_leads_and_the_next_takes_over_in_a_later_term_once_it_lets_go() {
        let Some(first) = database().await else {
            return;
        };
        let second = replica(&first).await;
        let held = lead_soon(&first).await.expect("the first replica leads at once");
        let waited = Cell::new(false);
        let cancel = CancellationToken::new();
        let taking = lead(&second, QUICK, &cancel, || waited.set(true));
        tokio::pin!(taking);
        assert!(
            tokio::time::timeout(Duration::from_millis(300), &mut taking)
                .await
                .is_err(),
            "the second waits while the first leads"
        );
        let term = held.term();
        drop(held);
        let next = taking.await.unwrap().expect("the second takes over");
        assert!(waited.get());
        assert_eq!(next.term(), term + 1);
    }

    #[tokio::test]
    async fn a_replica_that_waits_for_the_lead_stops_waiting_when_cancelled() {
        let Some(first) = database().await else {
            return;
        };
        let second = replica(&first).await;
        let _lead = lead_soon(&first).await.expect("the first replica leads at once");
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(lead(&second, QUICK, &cancel, || {}).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn the_writes_of_a_replica_whose_term_is_over_are_refused() {
        let Some(first) = database().await else {
            return;
        };
        let second = replica(&first).await;
        let held = lead_soon(&first).await.expect("the first replica leads at once");
        let write = "INSERT INTO zx_im_usage (day, platform, chat, tokens, cost) \
                     SELECT 1, 'slack', $1::text, 1, 0 WHERE zx_im_fence($2)";
        assert_eq!(first.write(write, &[&"C1"]).await.unwrap(), 1);
        drop(held);
        let _next = lead_soon(&second).await.expect("the second takes over");
        let refused = first.write(write, &[&"C2"]).await.unwrap_err();
        assert!(matches!(refused, Failure::Fenced), "{refused}");
        assert_eq!(second.write(write, &[&"C3"]).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn the_lead_is_lost_with_its_connection() {
        let Some(first) = database().await else {
            return;
        };
        let mut held = lead_soon(&first).await.expect("the replica leads at once");
        // The server ends the connection that holds the lock, as a failover of the database
        // would. The lock's 64-bit key shows as two halves.
        let ended = first
            .execute(
                "SELECT pg_terminate_backend(pid) FROM pg_locks, zx_im_leader \
                 WHERE locktype = 'advisory' AND granted \
                 AND classid = (lock_key >> 32)::oid AND objid = (lock_key & 4294967295)::oid",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(ended, 1);
        let why = tokio::time::timeout(Duration::from_secs(5), held.lost())
            .await
            .expect("the lead is reported lost");
        assert!(why.contains("the connection that holds the lead"), "{why}");
    }
}
