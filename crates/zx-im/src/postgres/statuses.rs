//! The statuses of runs in progress, a row per status message in `zx_im_statuses`.

use std::sync::Arc;

use async_trait::async_trait;
use tokio_postgres::Row;

use super::{Database, Failure};
use crate::{ConversationId, Destination, MessageId, MessageRef, SessionKey, StatusStore, ThreadId};

/// The statuses, kept in the database.
pub(crate) struct PostgresStatuses {
    database: Arc<Database>,
}

impl PostgresStatuses {
    pub(crate) fn new(database: Arc<Database>) -> Self {
        Self { database }
    }
}

#[async_trait]
impl StatusStore for PostgresStatuses {
    async fn insert(&self, key: &SessionKey, status: &MessageRef) -> Result<(), String> {
        let thread = key.thread.as_ref().map(|thread| thread.0.as_str());
        let status_thread = status.at.thread.as_ref().map(|thread| thread.0.as_str());
        self.database
            .write(
                "INSERT INTO zx_im_statuses \
                 (platform, status_conversation, status_thread, status_id, conversation, thread) \
                 SELECT $1::text, $2::text, $3::text, $4::text, $5::text, $6::text WHERE zx_im_fence($7) \
                 ON CONFLICT DO NOTHING",
                &[
                    &key.platform,
                    &status.at.conversation.0,
                    &status_thread,
                    &status.id.0,
                    &key.conversation.0,
                    &thread,
                ],
            )
            .await
            .map(drop)
            .map_err(|failure| format!("cannot keep status {}: {failure}", status.id))
    }

    async fn remove(&self, key: &SessionKey, status: &MessageRef) -> Result<(), String> {
        self.database
            .write(
                "DELETE FROM zx_im_statuses \
                 WHERE platform = $1 AND status_conversation = $2 AND status_id = $3 AND zx_im_fence($4)",
                &[&key.platform, &status.at.conversation.0, &status.id.0],
            )
            .await
            .map(drop)
            .map_err(|failure| format!("cannot forget status {}: {failure}", status.id))
    }

    async fn list(&self) -> Result<Vec<(SessionKey, MessageRef)>, String> {
        let read = async {
            let rows = self
                .database
                .query(
                    "SELECT platform, status_conversation, status_thread, status_id, conversation, thread \
                     FROM zx_im_statuses ORDER BY kept_order",
                    &[],
                )
                .await?;
            rows.iter().map(kept).collect::<Result<_, Failure>>()
        };
        read.await
            .map_err(|failure| format!("cannot read the statuses: {failure}"))
    }
}

/// The status `row` keeps, with the conversation of its run.
fn kept(row: &Row) -> Result<(SessionKey, MessageRef), Failure> {
    let key = SessionKey {
        platform: row.try_get(0)?,
        conversation: ConversationId(row.try_get(4)?),
        thread: row.try_get::<_, Option<String>>(5)?.map(ThreadId),
    };
    let status = MessageRef {
        at: Destination {
            conversation: ConversationId(row.try_get(1)?),
            thread: row.try_get::<_, Option<String>>(2)?.map(ThreadId),
        },
        id: MessageId(row.try_get(3)?),
    };
    Ok((key, status))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::leading;
    use super::*;

    /// The status `id` of a run in the conversation `thread` of chat `C1`, posted in that
    /// thread.
    fn status(thread: Option<&str>, id: &str) -> (SessionKey, MessageRef) {
        let at = Destination {
            conversation: ConversationId("C1".into()),
            thread: thread.map(|thread| ThreadId(thread.into())),
        };
        let key = SessionKey {
            platform: "slack".into(),
            conversation: at.conversation.clone(),
            thread: at.thread.clone(),
        };
        (
            key,
            MessageRef {
                at,
                id: MessageId(id.into()),
            },
        )
    }

    #[tokio::test]
    async fn statuses_are_listed_as_kept_until_they_are_removed() {
        let Some(database) = leading().await else {
            return;
        };
        let store = PostgresStatuses::new(database);
        let (in_thread, direct) = (status(Some("t1"), "s1"), status(None, "s2"));
        store.insert(&in_thread.0, &in_thread.1).await.unwrap();
        store.insert(&direct.0, &direct.1).await.unwrap();
        // Kept once, however often it is inserted.
        store.insert(&direct.0, &direct.1).await.unwrap();
        assert_eq!(store.list().await.unwrap(), [in_thread.clone(), direct.clone()]);

        store.remove(&in_thread.0, &in_thread.1).await.unwrap();
        // Forgetting what is not kept succeeds.
        store.remove(&in_thread.0, &in_thread.1).await.unwrap();
        assert_eq!(store.list().await.unwrap(), [direct]);
    }
}
