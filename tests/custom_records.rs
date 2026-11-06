//! The records extensions append to a session (P10 item 7, format v6): `custom` entries,
//! which stay out of model context, and `custom_message` entries, which the model sees as
//! user messages.
mod common;

use serde_json::json;

use common::{header, read_lines, summarize};
use zx_agent::ai::{Message, UserMessageKind};
use zx_agent::session::{CustomMessage, SESSION_VERSION, SessionHeader, SessionStore};

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
    assert_eq!(store.entries().len(), 4, "a new session needs no format upgrade");
}

#[tokio::test]
async fn an_older_session_is_upgraded_before_its_first_custom_record() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let mut store = SessionStore::create(&path, SessionHeader { version: 5, ..header() }).unwrap();
    store.append_message(Message::user("first")).await.unwrap();
    store.append_custom_entry("state", json!(1)).await.unwrap();
    store.append_custom_message(note("remember")).await.unwrap();
    drop(store);

    let lines = read_lines(&path);
    assert_eq!(lines[0]["version"], 5, "the header is never rewritten");
    assert_eq!(
        (lines[2]["type"].as_str(), lines[2]["version"].as_u64()),
        (Some("format_upgrade"), Some(6))
    );
    assert_eq!(lines[3]["type"], "custom");
    assert_eq!(
        (lines[3]["customType"].as_str(), lines[3]["data"].as_u64()),
        (Some("state"), Some(1))
    );
    assert_eq!(lines[4]["type"], "custom_message");
    assert_eq!(
        (
            lines[4]["customType"].as_str(),
            lines[4]["content"].as_str(),
            lines[4]["display"].as_bool(),
            lines[4]["details"]["n"].as_u64()
        ),
        (Some("note"), Some("remember"), Some(true), Some(1))
    );
    assert_eq!(lines[4]["parentId"], lines[3]["id"]);
    let reopened = SessionStore::open(&path).unwrap();
    assert_eq!(summarize(&reopened.messages()), ["user:first", "user:remember"]);
    assert_eq!(SESSION_VERSION, 6);
}

#[test]
fn a_custom_record_without_the_upgrade_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let header = json!({"type": "session", "version": 5, "id": "h", "timestamp": 1, "cwd": "/", "model": "m"});
    let custom = json!({"type": "custom", "id": "c1", "parentId": null, "timestamp": 2, "customType": "state"});
    std::fs::write(&path, format!("{header}\n{custom}\n")).unwrap();

    let error = match SessionStore::open(&path) {
        Ok(_) => panic!("a custom record needs the upgrade"),
        Err(error) => format!("{error:#}"),
    };

    assert!(error.contains("format upgrade to v6"), "{error}");
}

#[tokio::test]
async fn a_custom_record_needs_a_type_and_writes_nothing_without_one() {
    let mut store = SessionStore::in_memory(SessionHeader { version: 5, ..header() });
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
    assert!(store.entries().is_empty(), "not even an upgrade is written");
}
