//! Append-only compaction format, branch projection and legacy compatibility.

mod common;

use std::path::Path;

use serde_json::json;

use zx_agent::ai::{Message, SystemMessage, SystemPrompt, Usage, now_ms};
use zx_agent::session::{
    CompactionPlan, CompactionSummary, Entry, SESSION_VERSION, SessionHeader, SessionStore, SummaryFact, SummaryRequest,
};

use common::{header, read_lines, summarize};

fn summary(id: &str) -> CompactionSummary {
    CompactionSummary {
        facts: vec![SummaryFact {
            text: "The old observation".into(),
            source_entry_ids: vec![id.into()],
        }],
        inferences: vec!["An inference".into()],
        plans: vec!["Continue the task".into()],
    }
}

fn plan(store: &SessionStore, first_kept: &str) -> CompactionPlan {
    let source_entry_ids = store
        .raw_projection()
        .into_iter()
        .take_while(|item| item.entry_id != first_kept)
        .map(|item| item.entry_id)
        .collect();
    CompactionPlan {
        first_kept_entry_id: first_kept.into(),
        source_entry_ids,
        request: SummaryRequest {
            provider: "faux".into(),
            model: "m".into(),
            instructions: "Summarize".into(),
            max_tokens: 100,
        },
        input_bytes: 1000,
    }
}

fn seeded(store: &mut SessionStore) -> (String, String) {
    let old = store.append_message(Message::user("old evidence")).unwrap();
    let kept = store.append_message(Message::user("latest input")).unwrap();
    (old, kept)
}

#[test]
fn new_sessions_use_v4_and_legacy_upgrade_preserves_every_existing_byte() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(header().version, 4);
    for version in [1, 2] {
        let path = dir.path().join(format!("v{version}.jsonl"));
        let mut legacy_header = SessionHeader::new(Path::new("."), "m");
        legacy_header.version = version;
        let mut store = SessionStore::create(&path, legacy_header).unwrap();
        let (old, kept) = seeded(&mut store);
        drop(store);
        let before = std::fs::read(&path).unwrap();
        let mut store = SessionStore::open(&path).unwrap();
        let attempt = store.begin_compaction(plan(&store, &kept)).unwrap();
        store
            .append_compaction(&attempt, summary(&old), Usage::default())
            .unwrap();
        assert_eq!(store.header().version, version);
        assert_eq!(store.format_version(), 3);
        assert!(std::fs::read(&path).unwrap().starts_with(&before));
        let lines = read_lines(&path);
        assert_eq!(lines[3]["type"], "format_upgrade");
        assert_eq!(lines[4]["type"], "compaction_started");
        assert_eq!(lines[5]["type"], "compaction");
        assert_eq!(lines[5]["firstKeptEntryId"], kept);
        let reopened = SessionStore::open(&path).unwrap();
        assert_eq!(reopened.messages(), store.messages());
        assert_eq!(reopened.format_version(), 3);
    }
}

#[test]
fn summary_replaces_only_the_old_prefix_and_preserves_all_system_updates() {
    let mut store = SessionStore::in_memory(header());
    let first_system = Message::System(SystemMessage {
        sections: SystemPrompt::default().diff(&SystemPrompt::default().with_section("preamble", "first")),
        timestamp: now_ms(),
    });
    store.append_message(first_system.clone()).unwrap();
    let old = store.append_message(Message::user("observed something")).unwrap();
    let second_system = Message::System(SystemMessage {
        sections: SystemPrompt::default().diff(&SystemPrompt::default().with_section("environment", "updated")),
        timestamp: now_ms(),
    });
    store.append_message(second_system.clone()).unwrap();
    let kept = store.append_message(Message::user("latest input")).unwrap();
    let raw = store.raw_projection();
    let attempt = store.begin_compaction(plan(&store, &kept)).unwrap();
    let done = store
        .append_compaction(&attempt, summary(&old), Usage::default())
        .unwrap();
    let projection = store.projection();
    assert_eq!(
        projection
            .iter()
            .filter(|item| matches!(item.message, Message::System(_)))
            .map(|item| item.message.clone())
            .collect::<Vec<_>>(),
        [first_system, second_system]
    );
    assert_eq!(projection[2].entry_id, done);
    assert!(
        matches!(&projection[2].message, Message::User { content, .. } if content.contains(&old) && content.contains("Inferences (not independently verified)"))
    );
    assert_eq!(projection[3].entry_id, kept);
    assert_eq!(store.raw_projection(), raw);
}

#[test]
fn unfinished_and_failed_attempts_do_not_change_context() {
    let mut store = SessionStore::in_memory(header());
    let (_, kept) = seeded(&mut store);
    let before = store.messages();
    let attempt = store.begin_compaction(plan(&store, &kept)).unwrap();
    assert_eq!(store.messages(), before);
    store
        .fail_compaction(&attempt, "cancelled".into(), Usage::default(), None)
        .unwrap();
    assert_eq!(store.messages(), before);
    assert!(
        store
            .append_compaction(&attempt, summary("unknown"), Usage::default())
            .unwrap_err()
            .to_string()
            .contains("already finished")
    );
}

#[test]
fn summaries_are_branch_local_and_references_to_other_branches_are_rejected() {
    let mut store = SessionStore::in_memory(header());
    let (old, kept) = seeded(&mut store);
    let attempt = store.begin_compaction(plan(&store, &kept)).unwrap();
    let done = store
        .append_compaction(&attempt, summary(&old), Usage::default())
        .unwrap();
    let compacted = store.messages();
    store.branch(&old).unwrap();
    let other = store.append_message(Message::user("different branch")).unwrap();
    assert_eq!(store.messages().len(), 2);
    assert!(
        store
            .append_compaction(&attempt, summary(&old), Usage::default())
            .unwrap_err()
            .to_string()
            .contains("not on this branch")
    );
    let mut invalid = plan(&store, &other);
    invalid.first_kept_entry_id = kept;
    assert!(store.begin_compaction(invalid).is_err());
    store.branch(&done).unwrap();
    assert_eq!(store.messages(), compacted);
}

#[test]
fn editing_summarized_evidence_falls_back_to_originals_with_the_edit_applied() {
    let mut store = SessionStore::in_memory(header());
    let (old, kept) = seeded(&mut store);
    let attempt = store.begin_compaction(plan(&store, &kept)).unwrap();
    store
        .append_compaction(&attempt, summary(&old), Usage::default())
        .unwrap();
    store
        .append_context_edit(&old, Some("corrected observation".into()))
        .unwrap();
    assert!(store.latest_compaction().is_none());
    assert_eq!(
        summarize(&store.messages()),
        ["user:corrected observation", "user:latest input"]
    );
}

#[test]
fn omitting_the_first_retained_message_keeps_the_summary_in_place_of_the_old_prefix() {
    let mut store = SessionStore::in_memory(header());
    let (old, kept) = seeded(&mut store);
    let attempt = store.begin_compaction(plan(&store, &kept)).unwrap();
    store
        .append_compaction(&attempt, summary(&old), Usage::default())
        .unwrap();
    store.append_message(Message::user("after")).unwrap();
    store.append_context_edit(&kept, None).unwrap();
    // The retained message is not summarized evidence, so the summary stays valid.
    assert!(store.latest_compaction().is_some());
    let messages = summarize(&store.messages());
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert!(messages[0].starts_with("user:<context_summary>"), "{messages:?}");
    assert_eq!(messages[1], "user:after");
}

#[test]
fn a_summary_cannot_commit_after_its_source_was_edited_during_the_attempt() {
    let mut store = SessionStore::in_memory(header());
    let (old, kept) = seeded(&mut store);
    let attempt = store.begin_compaction(plan(&store, &kept)).unwrap();
    store
        .append_context_edit(&old, Some("corrected observation".into()))
        .unwrap();
    assert!(
        store
            .append_compaction(&attempt, summary(&old), Usage::default())
            .is_err(),
        "a stale summary must not hide a corrected source"
    );
}

#[test]
fn invalid_fact_references_or_empty_summaries_never_append_a_success_record() {
    let mut store = SessionStore::in_memory(header());
    let (_, kept) = seeded(&mut store);
    let attempt = store.begin_compaction(plan(&store, &kept)).unwrap();
    let count = store.entries().len();
    for invalid in [
        summary("unknown"),
        CompactionSummary {
            facts: vec![],
            inferences: vec![],
            plans: vec![],
        },
    ] {
        assert!(store.append_compaction(&attempt, invalid, Usage::default()).is_err());
        assert_eq!(store.entries().len(), count);
    }
}

#[test]
fn invalid_plan_does_not_even_append_an_upgrade() {
    let mut legacy = header();
    legacy.version = 2;
    let mut store = SessionStore::in_memory(legacy);
    let (_, kept) = seeded(&mut store);
    let mut invalid = plan(&store, &kept);
    invalid.source_entry_ids.push("invented".into());
    assert!(store.begin_compaction(invalid).is_err());
    assert_eq!(store.format_version(), 2);
    assert_eq!(store.entries().len(), 2);
}

#[test]
fn duplicate_ids_and_future_upgrades_are_rejected_without_modifying_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut store = SessionStore::create(&path, header()).unwrap();
    let (id, _) = seeded(&mut store);
    drop(store);
    let prefix = std::fs::read_to_string(&path).unwrap();
    for bad in [
        json!({"type":"format_upgrade","id":"future","parentId":id,"timestamp":0,"version":SESSION_VERSION + 1}),
        json!({"type":"format_upgrade","id":id,"parentId":null,"timestamp":0,"version":3}),
    ] {
        let content = format!("{prefix}{bad}\n");
        std::fs::write(&path, &content).unwrap();
        assert!(SessionStore::open(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
    }
}

#[test]
fn serialized_summary_records_use_camel_case_fields_and_snake_case_tags() {
    let mut store = SessionStore::in_memory(header());
    let (old, kept) = seeded(&mut store);
    let attempt = store.begin_compaction(plan(&store, &kept)).unwrap();
    store
        .append_compaction(&attempt, summary(&old), Usage::default())
        .unwrap();
    let Entry::Compaction(entry) = store.entries().last().unwrap() else {
        panic!("compaction");
    };
    let json = serde_json::to_value(store.entries().last().unwrap()).unwrap();
    assert_eq!(json["type"], "compaction");
    assert_eq!(json["attemptId"], attempt);
    assert_eq!(json["firstKeptEntryId"], kept);
    assert_eq!(json["summary"]["facts"][0]["sourceEntryIds"][0], old);
    assert_eq!(entry.summary.facts[0].text, "The old observation");
}

#[test]
fn a_retained_boundary_never_splits_an_assistant_tool_result_exchange() {
    use zx_agent::ai::{AssistantMessage, ContentBlock, StopReason, ToolResultMessage};
    let mut store = SessionStore::in_memory(header());
    let old = store.append_message(Message::user("old evidence")).unwrap();
    let mut assistant = AssistantMessage::failure("faux", "m", StopReason::ToolUse, "");
    assistant.error_message = None;
    assistant.content = vec![ContentBlock::ToolCall {
        id: "c".into(),
        name: "read_file".into(),
        arguments: json!({"path":"x"}),
        arguments_error: None,
    }];
    let call = store.append_message(Message::Assistant(assistant)).unwrap();
    let result = store
        .append_message(Message::ToolResult(ToolResultMessage {
            tool_call_id: "c".into(),
            tool_name: "read_file".into(),
            content: "value".into(),
            is_error: false,
            timestamp: 0,
            duration_ms: None,
        }))
        .unwrap();
    assert!(store.begin_compaction(plan(&store, &result)).is_err());
    let attempt = store.begin_compaction(plan(&store, &call)).unwrap();
    store
        .append_compaction(&attempt, summary(&old), Usage::default())
        .unwrap();
    let messages = store.messages();
    assert!(matches!(&messages[1], Message::Assistant(assistant) if assistant.tool_calls()[0].id == "c"));
    assert!(matches!(&messages[2], Message::ToolResult(result) if result.tool_call_id == "c"));
}
