//! The records extensions append to a session: `custom` entries, which stay out
//! of model context, and `custom_message` entries, which the model sees as user messages.
mod common;

use serde_json::json;

use common::{header, read_lines, summarize};
use zx_agent::ai::{Message, UserMessageKind};
use zx_agent::session::{CustomMessage, SessionStore};

fn note(text: &str) -> CustomMessage {
    CustomMessage {
        custom_type: "note".into(),
        content: text.into(),
        display: true,
        details: Some(json!({"n": 1})),
    }
}

#[tokio::test]
async fn custom_messages_join_the_projection_as_user_messages_and_custom_entries_stay_out() {
    let mut store = SessionStore::in_memory(header());
    store.append_message(Message::user("first")).await.unwrap();
    store.append_custom_entry("state", json!({"n": 1})).await.unwrap();
    let id = store.append_custom_message(note("remember")).await.unwrap();

    assert_eq!(summarize(&store.messages()), ["user:first", "user:remember"]);
    let projected = store.projection();
    assert_eq!(projected[1].entry_id, id);
    assert!(matches!(
        &projected[1].message,
        Message::User {
            kind: UserMessageKind::Custom,
            ..
        }
    ));
    // A context edit targets the record like any user message.
    store.append_context_edit(&id, None).await.unwrap();
    assert_eq!(summarize(&store.messages()), ["user:first"]);
    assert_eq!(store.entries().len(), 4);
}

#[tokio::test]
async fn custom_records_are_written_with_camel_case_fields_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let mut store = SessionStore::create(&path, header()).unwrap();
    store.append_message(Message::user("first")).await.unwrap();
    store.append_custom_entry("state", json!(1)).await.unwrap();
    store.append_custom_message(note("remember")).await.unwrap();
    drop(store);

    let lines = read_lines(&path);
    assert_eq!(lines[2]["type"], "custom");
    assert_eq!(
        (lines[2]["customType"].as_str(), lines[2]["data"].as_u64()),
        (Some("state"), Some(1))
    );
    assert_eq!(lines[3]["type"], "custom_message");
    assert_eq!(
        (
            lines[3]["customType"].as_str(),
            lines[3]["content"].as_str(),
            lines[3]["display"].as_bool(),
            lines[3]["details"]["n"].as_u64()
        ),
        (Some("note"), Some("remember"), Some(true), Some(1))
    );
    assert_eq!(lines[3]["parentId"], lines[2]["id"]);
    let reopened = SessionStore::open(&path).unwrap();
    assert_eq!(summarize(&reopened.messages()), ["user:first", "user:remember"]);
}

#[tokio::test]
async fn a_custom_record_needs_a_type_and_writes_nothing_without_one() {
    let mut store = SessionStore::in_memory(header());
    assert!(store.append_custom_entry(" ", json!(null)).await.is_err());
    assert!(
        store
            .append_custom_message(CustomMessage {
                custom_type: String::new(),
                ..note("x")
            })
            .await
            .is_err()
    );
    assert!(store.entries().is_empty());
}
