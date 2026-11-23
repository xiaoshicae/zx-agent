//! Each message until it is handled, a row per message in `zx_im_backlog`. A handled message
//! keeps its row, emptied, for [`HANDLED_FOR`], so that a delivery of it again is known.

use std::sync::Arc;

use async_trait::async_trait;
use tokio_postgres::Row;

use super::{Database, Failure, now};
use crate::backlog::HANDLED_FOR;
use crate::{Backlog, ConversationId, Kept, MessageId, MessageKey};

/// The backlog, kept in the database.
pub(crate) struct PostgresBacklog {
    database: Arc<Database>,
}

impl PostgresBacklog {
    pub(crate) fn new(database: Arc<Database>) -> Self {
        Self { database }
    }

    /// Remove the rows of messages handled longer ago than [`HANDLED_FOR`].
    async fn prune(&self) -> Result<(), String> {
        // Beyond i64 milliseconds, 24 hours is a rounding error.
        let window = i64::try_from(HANDLED_FOR.as_millis()).unwrap_or(i64::MAX);
        let before = now().saturating_sub(window);
        self.database
            .execute("DELETE FROM zx_im_backlog WHERE completed_ms < $1", &[&before])
            .await
            .map(drop)
            .map_err(|failure| format!("cannot prune the backlog: {failure}"))
    }
}

#[async_trait]
impl Backlog for PostgresBacklog {
    async fn keep(&self, key: &MessageKey, record: &str) -> Result<Kept, String> {
        let kept = self
            .database
            .execute(
                "INSERT INTO zx_im_backlog (platform, conversation, message, record) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (platform, conversation, message) DO NOTHING",
                &[&key.platform, &key.conversation.0, &key.message.0, &record],
            )
            .await
            .map_err(|failure| format!("cannot keep message {}: {failure}", key.message))?;
        Ok(if kept == 1 { Kept::New } else { Kept::Again })
    }

    /// Known as handled also when it was not kept, as [`keep`](Backlog::keep) requires.
    async fn complete(&self, key: &MessageKey) -> Result<(), String> {
        let now = now();
        self.database
            .execute(
                "INSERT INTO zx_im_backlog (platform, conversation, message, record, completed_ms) \
                 VALUES ($1, $2, $3, '', $4) \
                 ON CONFLICT (platform, conversation, message) DO UPDATE SET record = '', completed_ms = $4",
                &[&key.platform, &key.conversation.0, &key.message.0, &now],
            )
            .await
            .map_err(|failure| format!("cannot complete message {}: {failure}", key.message))?;
        // The message is handled whatever comes of this: a row not pruned now is at the next
        // completion, and only keeps its message known longer.
        self.prune().await.ok();
        Ok(())
    }

    async fn pending(&self) -> Result<Vec<(MessageKey, String)>, String> {
        let read = async {
            let rows = self
                .database
                .query(
                    "SELECT platform, conversation, message, record FROM zx_im_backlog \
                     WHERE completed_ms IS NULL ORDER BY kept_order",
                    &[],
                )
                .await?;
            rows.iter().map(pending).collect::<Result<_, Failure>>()
        };
        read.await
            .map_err(|failure| format!("cannot read the backlog: {failure}"))
    }
}

/// The message pending that `row` keeps, with its record.
fn pending(row: &Row) -> Result<(MessageKey, String), Failure> {
    let key = MessageKey {
        platform: row.try_get(0)?,
        conversation: ConversationId(row.try_get(1)?),
        message: MessageId(row.try_get(2)?),
    };
    Ok((key, row.try_get(3)?))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::database;
    use super::*;
    use crate::test_support::message_key as key;

    #[tokio::test]
    async fn a_message_is_kept_once_and_known_after_it_is_handled() {
        let Some(database) = database().await else {
            return;
        };
        let backlog = PostgresBacklog::new(database);
        assert_eq!(backlog.keep(&key("m1"), "one").await, Ok(Kept::New));
        assert_eq!(backlog.keep(&key("m2"), "two").await, Ok(Kept::New));
        assert_eq!(backlog.keep(&key("m1"), "one").await, Ok(Kept::Again));
        assert_eq!(
            backlog.pending().await.unwrap(),
            [(key("m1"), "one".to_string()), (key("m2"), "two".to_string())]
        );
        backlog.complete(&key("m1")).await.unwrap();
        assert_eq!(backlog.pending().await.unwrap(), [(key("m2"), "two".to_string())]);
        assert_eq!(backlog.keep(&key("m1"), "one").await, Ok(Kept::Again));
        // Completing what was never kept succeeds, and knows the message from then on.
        backlog.complete(&key("m3")).await.unwrap();
        assert_eq!(backlog.keep(&key("m3"), "three").await, Ok(Kept::Again));
        assert_eq!(backlog.pending().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_message_handled_longer_ago_than_a_platform_redelivers_is_forgotten() {
        let Some(database) = database().await else {
            return;
        };
        let backlog = PostgresBacklog::new(Arc::clone(&database));
        backlog.complete(&key("old")).await.unwrap();
        let long_ago = now() - i64::try_from(HANDLED_FOR.as_millis()).unwrap() - 1;
        database
            .execute("UPDATE zx_im_backlog SET completed_ms = $1", &[&long_ago])
            .await
            .unwrap();
        backlog.complete(&key("new")).await.unwrap();
        assert_eq!(backlog.keep(&key("old"), "again").await, Ok(Kept::New));
        assert_eq!(backlog.keep(&key("new"), "again").await, Ok(Kept::Again));
        // What a handled message was is not kept.
        let records = database
            .query("SELECT record FROM zx_im_backlog WHERE message = 'new'", &[])
            .await
            .unwrap();
        assert_eq!(records[0].get::<_, String>(0), "");
    }
}
