use serde_json::json;

use super::{CompactionPlan, CompactionSummary, SummarizedBy};
use crate::ai::{
    AssistantMessage, ContentBlock, Message, Request, StopReason, SystemMessage, SystemPrompt, ToolCall,
    ToolResultMessage, Usage, now_ms,
};
use crate::transcript::test_support::{header, loaded, plan, records_of, seeded, summary};
use crate::transcript::{Entry, RequestSource, Transcript};

/// Each message as its role and what it says, such as `user:latest input`, as the integration
/// tests' `zx_testing::summarize` writes them.
fn summarize(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .map(|message| match message {
            Message::System(system) => {
                let names: Vec<&str> = system.sections.iter().map(|change| change.name.as_str()).collect();
                format!("system:{}", names.join(","))
            }
            Message::User { content, .. } => format!("user:{content}"),
            Message::Assistant(answer) => {
                let calls: Vec<String> = answer.tool_calls().into_iter().map(|call| call.id).collect();
                if calls.is_empty() {
                    format!("assistant:{}", answer.text())
                } else {
                    format!("assistant:calls[{}]", calls.join(","))
                }
            }
            Message::ToolResult(result) => format!(
                "tool:{}={}{}",
                result.tool_call_id,
                result.content,
                if result.is_error { "!" } else { "" }
            ),
        })
        .collect()
}

#[tokio::test]
async fn summary_replaces_only_the_old_prefix_and_preserves_all_system_updates() {
    let mut store = Transcript::in_memory(header());
    let first_system = Message::System(SystemMessage {
        sections: SystemPrompt::default().diff(&SystemPrompt::default().with_section("preamble", "first")),
        timestamp: now_ms(),
    });
    store.append_message(first_system.clone()).await.unwrap();
    let old = store.append_message(Message::user("observed something")).await.unwrap();
    let second_system = Message::System(SystemMessage {
        sections: SystemPrompt::default().diff(&SystemPrompt::default().with_section("environment", "updated")),
        timestamp: now_ms(),
    });
    store.append_message(second_system.clone()).await.unwrap();
    let kept = store.append_message(Message::user("latest input")).await.unwrap();
    let raw = store.raw_projection();
    let started = store.begin_compaction(plan(&store, &kept)).await.unwrap();
    let done = store
        .append_compaction(&started, summary(&old), Usage::default())
        .await
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

#[tokio::test]
async fn unfinished_and_failed_attempts_do_not_change_context() {
    let mut store = Transcript::in_memory(header());
    let (_, kept) = seeded(&mut store).await;
    let before = store.messages();
    let started = store.begin_compaction(plan(&store, &kept)).await.unwrap();
    assert_eq!(store.messages(), before);
    store
        .fail_compaction(&started, "cancelled".into(), Usage::default(), None)
        .await
        .unwrap();
    assert_eq!(store.messages(), before);
    assert!(
        store
            .append_compaction(&started, summary("unknown"), Usage::default())
            .await
            .unwrap_err()
            .to_string()
            .contains("already finished")
    );
}

#[tokio::test]
async fn a_completed_compaction_accepts_neither_a_second_result_nor_a_failure() {
    let mut store = Transcript::in_memory(header());
    let (old, kept) = seeded(&mut store).await;
    let started = store.begin_compaction(plan(&store, &kept)).await.unwrap();
    store
        .append_compaction(&started, summary(&old), Usage::default())
        .await
        .unwrap();
    let recorded = store.entries().len();

    let again = store
        .append_compaction(&started, summary(&old), Usage::default())
        .await
        .unwrap_err();
    let failed = store
        .fail_compaction(&started, "late".into(), Usage::default(), None)
        .await
        .unwrap_err();

    assert!(again.to_string().contains("already finished"), "{again:#}");
    assert!(failed.to_string().contains("already finished"), "{failed:#}");
    assert_eq!(store.entries().len(), recorded);
}

#[tokio::test]
async fn summaries_are_branch_local_and_references_to_other_branches_are_rejected() {
    let mut store = Transcript::in_memory(header());
    let (old, kept) = seeded(&mut store).await;
    let started = store.begin_compaction(plan(&store, &kept)).await.unwrap();
    let done = store
        .append_compaction(&started, summary(&old), Usage::default())
        .await
        .unwrap();
    let compacted = store.messages();
    store.branch(&old).unwrap();
    let other = store.append_message(Message::user("different branch")).await.unwrap();
    assert_eq!(store.messages().len(), 2);
    assert!(
        store
            .append_compaction(&started, summary(&old), Usage::default())
            .await
            .unwrap_err()
            .to_string()
            .contains("not on this branch")
    );
    let mut invalid = plan(&store, &other);
    invalid.first_kept_entry_id = kept;
    assert!(store.begin_compaction(invalid).await.is_err());
    store.branch(&done).unwrap();
    assert_eq!(store.messages(), compacted);
}

#[tokio::test]
async fn editing_summarized_evidence_falls_back_to_originals_with_the_edit_applied() {
    let mut store = Transcript::in_memory(header());
    let (old, kept) = seeded(&mut store).await;
    let started = store.begin_compaction(plan(&store, &kept)).await.unwrap();
    store
        .append_compaction(&started, summary(&old), Usage::default())
        .await
        .unwrap();
    store
        .append_context_edit(&old, Some("corrected observation".into()))
        .await
        .unwrap();
    assert!(store.latest_compaction().is_none());
    assert_eq!(
        summarize(&store.messages()),
        ["user:corrected observation", "user:latest input"]
    );
}

#[tokio::test]
async fn omitting_the_first_retained_message_keeps_the_summary_in_place_of_the_old_prefix() {
    let mut store = Transcript::in_memory(header());
    let (old, kept) = seeded(&mut store).await;
    let started = store.begin_compaction(plan(&store, &kept)).await.unwrap();
    store
        .append_compaction(&started, summary(&old), Usage::default())
        .await
        .unwrap();
    store.append_message(Message::user("after")).await.unwrap();
    store.append_context_edit(&kept, None).await.unwrap();
    // The retained message is not summarized evidence, so the summary stays valid.
    assert!(store.latest_compaction().is_some());
    let messages = summarize(&store.messages());
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert!(messages[0].starts_with("user:<context_summary>"), "{messages:?}");
    assert_eq!(messages[1], "user:after");
}

#[tokio::test]
async fn a_summary_cannot_commit_after_its_source_was_edited_during_the_attempt() {
    let mut store = Transcript::in_memory(header());
    let (old, kept) = seeded(&mut store).await;
    let started = store.begin_compaction(plan(&store, &kept)).await.unwrap();
    store
        .append_context_edit(&old, Some("corrected observation".into()))
        .await
        .unwrap();
    assert!(
        store
            .append_compaction(&started, summary(&old), Usage::default())
            .await
            .is_err(),
        "a stale summary must not hide a corrected source"
    );
}

#[tokio::test]
async fn invalid_fact_references_or_empty_summaries_never_append_a_success_record() {
    let mut store = Transcript::in_memory(header());
    let (_, kept) = seeded(&mut store).await;
    let started = store.begin_compaction(plan(&store, &kept)).await.unwrap();
    let count = store.entries().len();
    for invalid in [
        summary("unknown"),
        CompactionSummary {
            facts: vec![],
            inferences: vec![],
            plans: vec![],
        },
    ] {
        assert!(
            store
                .append_compaction(&started, invalid, Usage::default())
                .await
                .is_err()
        );
        assert_eq!(store.entries().len(), count);
    }
}

#[tokio::test]
async fn an_invalid_plan_writes_nothing() {
    let mut store = Transcript::in_memory(header());
    let (_, kept) = seeded(&mut store).await;
    let mut invalid = plan(&store, &kept);
    invalid.source_entry_ids.push("invented".into());
    assert!(store.begin_compaction(invalid).await.is_err());
    assert_eq!(store.entries().len(), 2);
}

#[tokio::test]
async fn serialized_summary_records_use_camel_case_fields_and_snake_case_tags() {
    let mut store = Transcript::in_memory(header());
    let (old, kept) = seeded(&mut store).await;
    let started = store.begin_compaction(plan(&store, &kept)).await.unwrap();
    store
        .append_compaction(&started, summary(&old), Usage::default())
        .await
        .unwrap();
    let Entry::Compaction(entry) = store.entries().last().unwrap() else {
        panic!("compaction");
    };
    let json = serde_json::to_value(store.entries().last().unwrap()).unwrap();
    assert_eq!(json["type"], "compaction");
    assert_eq!(json["compactionId"], started);
    assert_eq!(json["firstKeptEntryId"], kept);
    assert_eq!(json["summary"]["facts"][0]["sourceEntryIds"][0], old);
    assert_eq!(entry.summary.facts[0].text, "The old observation");
}

#[tokio::test]
async fn a_start_records_either_the_request_or_the_name_of_the_summarizer() {
    let mut store = Transcript::in_memory(header());
    let (old, kept) = seeded(&mut store).await;
    let by_model = plan(&store, &kept);
    let asked = store.begin_compaction(by_model).await.unwrap();
    store
        .fail_compaction(&asked, "invalid".into(), Usage::default(), None)
        .await
        .unwrap();
    let by_summarizer = CompactionPlan {
        by: SummarizedBy::Summarizer("notes".into()),
        ..plan(&store, &kept)
    };
    let made = store.begin_compaction(by_summarizer).await.unwrap();
    store
        .append_compaction(&made, summary(&old), Usage::default())
        .await
        .unwrap();

    // The records as the session's log was given them.
    let records = records_of(&store);
    let lines: Vec<serde_json::Value> = records
        .iter()
        .map(|record| serde_json::from_str(record).unwrap())
        .collect();
    let starts: Vec<_> = lines
        .iter()
        .filter(|line| line["type"] == "compaction_started")
        .collect();
    assert_eq!(starts[0]["request"]["maxTokens"], 100);
    assert!(starts[0].get("summarizer").is_none());
    assert_eq!(starts[1]["summarizer"], "notes");
    assert!(starts[1].get("request").is_none());
    // Both load again, each as it was written.
    let reopened = Transcript::snapshot(loaded(store.header().clone(), records)).unwrap();
    let recorded: Vec<&SummarizedBy> = reopened
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            Entry::CompactionStarted(started) => Some(&started.by),
            _ => None,
        })
        .collect();
    assert!(matches!(recorded[0], SummarizedBy::Request(request) if request.max_tokens == 100));
    assert_eq!(recorded[1], &SummarizedBy::Summarizer("notes".into()));
    assert_eq!(reopened.messages().len(), 2, "the summary and the latest input");
}

#[tokio::test]
async fn a_summarizer_needs_a_name_and_has_no_request_in_the_session() {
    let mut store = Transcript::in_memory(header());
    let (_, kept) = seeded(&mut store).await;
    let nameless = CompactionPlan {
        by: SummarizedBy::Summarizer(" ".into()),
        ..plan(&store, &kept)
    };
    let error = store.begin_compaction(nameless).await.unwrap_err();
    assert!(format!("{error:#}").contains("the summarizer has no name"), "{error:#}");

    let named = CompactionPlan {
        by: SummarizedBy::Summarizer("notes".into()),
        ..plan(&store, &kept)
    };
    let started = store.begin_compaction(named).await.unwrap();
    let request = Request {
        model: "m".into(),
        messages: Vec::new(),
        tools: Vec::new(),
        max_tokens: Some(100),
        reasoning: None,
        cache_key: None,
    };
    let source = RequestSource::Summary {
        compaction_id: started,
        system_timestamp: now_ms(),
        user_timestamp: now_ms(),
    };
    let error = store
        .record_request(&request, "faux", json!({}), source)
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("is summarized by \"notes\", not by a request"),
        "{error:#}"
    );
}

/// An answer that calls `read_file` as `id`, and that call's result.
fn exchange(id: &str) -> [Message; 2] {
    let mut assistant = AssistantMessage::failure("faux", "m", StopReason::ToolUse, "");
    assistant.error_message = None;
    assistant.content = vec![ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: "read_file".into(),
        arguments: json!({"path": "x"}),
        arguments_error: None,
    })];
    let result = ToolResultMessage {
        tool_call_id: id.into(),
        tool_name: "read_file".into(),
        content: format!("value of {id}"),
        is_error: false,
        timestamp: 0,
        duration_ms: None,
        details: None,
        usage: None,
    };
    [Message::Assistant(assistant), Message::ToolResult(result)]
}

/// A session whose latest turn, "the task", ran two calls, c1 and c2, after an earlier
/// question: the IDs of the earlier question, of the task, and of the two calls.
async fn a_turn_with_two_calls() -> (Transcript, [String; 4]) {
    let mut store = Transcript::in_memory(header());
    let earlier = store
        .append_message(Message::user("an earlier question"))
        .await
        .unwrap();
    let task = store.append_message(Message::user("the task")).await.unwrap();
    let [call, result] = exchange("c1");
    let first_call = store.append_message(call).await.unwrap();
    store.append_message(result).await.unwrap();
    let [call, result] = exchange("c2");
    let second_call = store.append_message(call).await.unwrap();
    store.append_message(result).await.unwrap();
    (store, [earlier, task, first_call, second_call])
}

#[tokio::test]
async fn a_compaction_inside_a_turn_keeps_the_input_of_that_turn_before_its_summary() {
    let (mut store, [earlier, task, first_call, kept]) = a_turn_with_two_calls().await;

    // Only the latest input before the boundary can be kept.
    let wrong = CompactionPlan {
        kept_input_entry_id: Some(earlier.clone()),
        ..plan(&store, &kept)
    };
    let error = store.begin_compaction(wrong).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("is not the latest input before its boundary"),
        "{error:#}"
    );

    let inside = CompactionPlan {
        kept_input_entry_id: Some(task.clone()),
        ..plan(&store, &kept)
    };
    let started = store.begin_compaction(inside).await.unwrap();
    store
        .append_compaction(&started, summary(&first_call), Usage::default())
        .await
        .unwrap();

    let texts: Vec<String> = store
        .messages()
        .iter()
        .map(|message| match message {
            Message::User { content, .. } => content.lines().next().unwrap_or_default().to_string(),
            Message::Assistant(answer) => format!("calls {}", answer.tool_calls()[0].id),
            Message::ToolResult(result) => result.content.clone(),
            Message::System(_) => "system".to_string(),
        })
        .collect();
    assert_eq!(texts, ["the task", "<context_summary>", "calls c2", "value of c2"]);
    let lines: Vec<serde_json::Value> = store
        .entries()
        .iter()
        .map(|entry| serde_json::to_value(entry).unwrap())
        .collect();
    let recorded = lines.iter().find(|line| line["type"] == "compaction_started").unwrap();
    assert_eq!(recorded["keptInputEntryId"], task);
}

#[tokio::test]
async fn a_retained_boundary_never_splits_an_assistant_tool_result_exchange() {
    let mut store = Transcript::in_memory(header());
    let old = store.append_message(Message::user("old evidence")).await.unwrap();
    let mut assistant = AssistantMessage::failure("faux", "m", StopReason::ToolUse, "");
    assistant.error_message = None;
    assistant.content = vec![ContentBlock::ToolCall(ToolCall {
        id: "c".into(),
        name: "read_file".into(),
        arguments: json!({"path":"x"}),
        arguments_error: None,
    })];
    let call = store.append_message(Message::Assistant(assistant)).await.unwrap();
    let result = store
        .append_message(Message::ToolResult(ToolResultMessage {
            tool_call_id: "c".into(),
            tool_name: "read_file".into(),
            content: "value".into(),
            is_error: false,
            timestamp: 0,
            duration_ms: None,
            details: None,
            usage: None,
        }))
        .await
        .unwrap();
    assert!(store.begin_compaction(plan(&store, &result)).await.is_err());
    let started = store.begin_compaction(plan(&store, &call)).await.unwrap();
    store
        .append_compaction(&started, summary(&old), Usage::default())
        .await
        .unwrap();
    let messages = store.messages();
    assert!(matches!(&messages[1], Message::Assistant(assistant) if assistant.tool_calls()[0].id == "c"));
    assert!(matches!(&messages[2], Message::ToolResult(result) if result.tool_call_id == "c"));
}
