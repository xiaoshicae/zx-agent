//! What the work of each day used, a row per day and chat in `zx_im_usage`.

use std::sync::Arc;

use async_trait::async_trait;
use tokio_postgres::Row;

use super::{Database, Failure, bigint};
use crate::{ChatUsage, ConversationId, UsageStore};

/// The daily usage, kept in the database.
pub(crate) struct PostgresUsage {
    database: Arc<Database>,
}

impl PostgresUsage {
    pub(crate) fn new(database: Arc<Database>) -> Self {
        Self { database }
    }
}

#[async_trait]
impl UsageStore for PostgresUsage {
    async fn day(&self, day: u64) -> Result<Vec<ChatUsage>, String> {
        let read = async {
            let number = bigint(day, "the day")?;
            let rows = self
                .database
                .query(
                    "SELECT platform, chat, tokens, cost FROM zx_im_usage WHERE day = $1 ORDER BY platform, chat",
                    &[&number],
                )
                .await?;
            rows.iter().map(used).collect::<Result<_, Failure>>()
        };
        read.await
            .map_err(|failure| format!("cannot read what day {day} used: {failure}"))
    }

    async fn add(&self, day: u64, used: &ChatUsage) -> Result<(), String> {
        let write = async {
            let (number, tokens) = (bigint(day, "the day")?, bigint(used.tokens, "the tokens")?);
            self.database
                .execute(
                    "INSERT INTO zx_im_usage (day, platform, chat, tokens, cost) VALUES ($1, $2, $3, $4, $5) \
                     ON CONFLICT (day, platform, chat) DO UPDATE \
                     SET tokens = zx_im_usage.tokens + EXCLUDED.tokens, cost = zx_im_usage.cost + EXCLUDED.cost",
                    &[&number, &used.platform, &used.chat.0, &tokens, &used.cost],
                )
                .await
        };
        write
            .await
            .map(drop)
            .map_err(|failure| format!("cannot keep what day {day} used: {failure}"))
    }
}

/// What a chat used on a day, as `row` keeps it.
fn used(row: &Row) -> Result<ChatUsage, Failure> {
    let tokens: i64 = row.try_get(2)?;
    Ok(ChatUsage {
        platform: row.try_get(0)?,
        chat: ConversationId(row.try_get(1)?),
        tokens: u64::try_from(tokens).map_err(|_| Failure::Other(format!("{tokens} tokens is no count")))?,
        cost: row.try_get(3)?,
    })
}

#[cfg(test)]
mod tests {
    use super::super::test_support::database;
    use super::*;

    fn used(chat: &str, tokens: u64, cost: f64) -> ChatUsage {
        ChatUsage {
            platform: "slack".into(),
            chat: ConversationId(chat.into()),
            tokens,
            cost,
        }
    }

    #[tokio::test]
    async fn what_each_chat_used_is_added_up_per_day() {
        let Some(database) = database().await else {
            return;
        };
        let store = PostgresUsage::new(database);
        store.add(7, &used("C2", 10, 0.5)).await.unwrap();
        store.add(7, &used("C1", 100, 1.0)).await.unwrap();
        store.add(7, &used("C1", 20, 0.25)).await.unwrap();
        store.add(8, &used("C1", 1, 0.0)).await.unwrap();
        assert_eq!(
            store.day(7).await.unwrap(),
            [used("C1", 120, 1.25), used("C2", 10, 0.5)]
        );
        assert!(store.day(6).await.unwrap().is_empty());
    }
}
