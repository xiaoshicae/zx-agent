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
async fn a_compaction_replaces_only_the_old_prefix_and_preserves_all_system_updates() {
    let mut transcript = Transcript::in_memory(header());
    let first_system = Message::System(SystemMessage {
        sections: SystemPrompt::default().diff(&SystemPrompt::default().with_section("preamble", "first")),
        timestamp: now_ms(),
    });
    transcript.append_message(first_system.clone()).await.unwrap();
    let old = transcript
        .append_message(Message::user("observed something"))
        .await
        .unwrap();
    let second_system = Message::System(SystemMessage {
        sections: SystemPrompt::default().diff(&SystemPrompt::default().with_section("environment", "updated")),
        timestamp: now_ms(),
    });
    transcript.append_message(second_system.clone()).await.unwrap();
    let kept = transcript.append_message(Message::user("latest input")).await.unwrap();
    let raw = transcript.raw_projection();
    let started = transcript.begin_compaction(plan(&transcript, &kept)).await.unwrap();
    let done = transcript
        .append_compaction(&started, summary(&old), Usage::default())
        .await
        .unwrap();
    let projection = transcript.projection();
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
    assert_eq!(transcript.raw_projection(), raw);
}

#[tokio::test]
async fn unfinished_and_failed_compactions_do_not_change_the_context() {
    let mut transcript = Transcript::in_memory(header());
    let (_, kept) = seeded(&mut transcript).await;
    let before = transcript.messages();
    let started = transcript.begin_compaction(plan(&transcript, &kept)).await.unwrap();
    assert_eq!(transcript.messages(), before);
    transcript
        .fail_compaction(&started, "cancelled".into(), Usage::default(), None)
        .await
        .unwrap();
    assert_eq!(transcript.messages(), before);
    assert!(
        transcript
            .append_compaction(&started, summary("unknown"), Usage::default())
            .await
            .unwrap_err()
            .to_string()
            .contains("already finished")
    );
}

#[tokio::test]
async fn a_completed_compaction_accepts_neither_a_second_result_nor_a_failure() {
    let mut transcript = Transcript::in_memory(header());
    let (old, kept) = seeded(&mut transcript).await;
    let started = transcript.begin_compaction(plan(&transcript, &kept)).await.unwrap();
    transcript
        .append_compaction(&started, summary(&old), Usage::default())
        .await
        .unwrap();
    let recorded = transcript.entries().len();

    let again = transcript
        .append_compaction(&started, summary(&old), Usage::default())
        .await
        .unwrap_err();
    let failed = transcript
        .fail_compaction(&started, "late".into(), Usage::default(), None)
        .await
        .unwrap_err();

    assert!(again.to_string().contains("already finished"), "{again:#}");
    assert!(failed.to_string().contains("already finished"), "{failed:#}");
    assert_eq!(transcript.entries().len(), recorded);
}

#[tokio::test]
async fn compactions_are_branch_local_and_references_to_other_branches_are_rejected() {
    let mut transcript = Transcript::in_memory(header());
    let (old, kept) = seeded(&mut transcript).await;
    let started = transcript.begin_compaction(plan(&transcript, &kept)).await.unwrap();
    let done = transcript
        .append_compaction(&started, summary(&old), Usage::default())
        .await
        .unwrap();
    let compacted = transcript.messages();
    transcript.branch(&old).unwrap();
    let other = transcript
        .append_message(Message::user("different branch"))
        .await
        .unwrap();
    assert_eq!(transcript.messages().len(), 2);
    assert!(
        transcript
            .append_compaction(&started, summary(&old), Usage::default())
            .await
            .unwrap_err()
            .to_string()
            .contains("not on this branch")
    );
    let mut invalid = plan(&transcript, &other);
    invalid.first_kept_entry_id = kept;
    assert!(transcript.begin_compaction(invalid).await.is_err());
    transcript.branch(&done).unwrap();
    assert_eq!(transcript.messages(), compacted);
}

#[tokio::test]
async fn editing_summarized_evidence_falls_back_to_originals_with_the_edit_applied() {
    let mut transcript = Transcript::in_memory(header());
    let (old, kept) = seeded(&mut transcript).await;
    let started = transcript.begin_compaction(plan(&transcript, &kept)).await.unwrap();
    transcript
        .append_compaction(&started, summary(&old), Usage::default())
        .await
        .unwrap();
    transcript
        .append_context_edit(&old, Some("corrected observation".into()))
        .await
        .unwrap();
    assert!(transcript.latest_compaction().is_none());
    assert_eq!(
        summarize(&transcript.messages()),
        ["user:corrected observation", "user:latest input"]
    );
}

#[tokio::test]
async fn omitting_the_first_kept_message_keeps_the_summary_in_place_of_the_old_prefix() {
    let mut transcript = Transcript::in_memory(header());
    let (old, kept) = seeded(&mut transcript).await;
    let started = transcript.begin_compaction(plan(&transcript, &kept)).await.unwrap();
    transcript
        .append_compaction(&started, summary(&old), Usage::default())
        .await
        .unwrap();
    transcript.append_message(Message::user("after")).await.unwrap();
    transcript.append_context_edit(&kept, None).await.unwrap();
    // The kept message is not summarized evidence, so the summary stays valid.
    assert!(transcript.latest_compaction().is_some());
    let messages = summarize(&transcript.messages());
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert!(messages[0].starts_with("user:<context_summary>"), "{messages:?}");
    assert_eq!(messages[1], "user:after");
}

#[tokio::test]
async fn a_compaction_cannot_finish_after_its_source_was_edited_during_the_summary_request() {
    let mut transcript = Transcript::in_memory(header());
    let (old, kept) = seeded(&mut transcript).await;
    let started = transcript.begin_compaction(plan(&transcript, &kept)).await.unwrap();
    transcript
        .append_context_edit(&old, Some("corrected observation".into()))
        .await
        .unwrap();
    assert!(
        transcript
            .append_compaction(&started, summary(&old), Usage::default())
            .await
            .is_err(),
        "a stale summary must not hide a corrected source"
    );
}

#[tokio::test]
async fn invalid_fact_references_or_empty_summaries_never_append_a_success_record() {
    let mut transcript = Transcript::in_memory(header());
    let (_, kept) = seeded(&mut transcript).await;
    let started = transcript.begin_compaction(plan(&transcript, &kept)).await.unwrap();
    let count = transcript.entries().len();
    for invalid in [
        summary("unknown"),
        CompactionSummary {
            facts: vec![],
            inferences: vec![],
            plans: vec![],
        },
    ] {
        assert!(
            transcript
                .append_compaction(&started, invalid, Usage::default())
                .await
                .is_err()
        );
        assert_eq!(transcript.entries().len(), count);
    }
}

#[tokio::test]
async fn an_invalid_plan_writes_nothing() {
    let mut transcript = Transcript::in_memory(header());
    let (_, kept) = seeded(&mut transcript).await;
    let mut invalid = plan(&transcript, &kept);
    invalid.source_entry_ids.push("invented".into());
    assert!(transcript.begin_compaction(invalid).await.is_err());
    assert_eq!(transcript.entries().len(), 2);
}

#[tokio::test]
async fn serialized_compaction_records_use_camel_case_fields_and_snake_case_tags() {
    let mut transcript = Transcript::in_memory(header());
    let (old, kept) = seeded(&mut transcript).await;
    let started = transcript.begin_compaction(plan(&transcript, &kept)).await.unwrap();
    transcript
        .append_compaction(&started, summary(&old), Usage::default())
        .await
        .unwrap();
    let Entry::Compaction(entry) = transcript.entries().last().unwrap() else {
        panic!("compaction");
    };
    let json = serde_json::to_value(transcript.entries().last().unwrap()).unwrap();
    assert_eq!(json["type"], "compaction");
    assert_eq!(json["compactionId"], started);
    assert_eq!(json["firstKeptEntryId"], kept);
    assert_eq!(json["summary"]["facts"][0]["sourceEntryIds"][0], old);
    assert_eq!(entry.summary.facts[0].text, "The old observation");
}

#[tokio::test]
async fn a_start_records_either_the_request_or_the_name_of_the_summarizer() {
    let mut transcript = Transcript::in_memory(header());
    let (old, kept) = seeded(&mut transcript).await;
    let by_model = plan(&transcript, &kept);
    let asked = transcript.begin_compaction(by_model).await.unwrap();
    transcript
        .fail_compaction(&asked, "invalid".into(), Usage::default(), None)
        .await
        .unwrap();
    let by_summarizer = CompactionPlan {
        by: SummarizedBy::Summarizer("notes".into()),
        ..plan(&transcript, &kept)
    };
    let made = transcript.begin_compaction(by_summarizer).await.unwrap();
    transcript
        .append_compaction(&made, summary(&old), Usage::default())
        .await
        .unwrap();

    // The records as the session's log was given them.
    let records = records_of(&transcript);
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
    let reopened = Transcript::snapshot(loaded(transcript.header().clone(), records)).unwrap();
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
    let mut transcript = Transcript::in_memory(header());
    let (_, kept) = seeded(&mut transcript).await;
    let nameless = CompactionPlan {
        by: SummarizedBy::Summarizer(" ".into()),
        ..plan(&transcript, &kept)
    };
    let error = transcript.begin_compaction(nameless).await.unwrap_err();
    assert!(format!("{error:#}").contains("the summarizer has no name"), "{error:#}");

    let named = CompactionPlan {
        by: SummarizedBy::Summarizer("notes".into()),
        ..plan(&transcript, &kept)
    };
    let started = transcript.begin_compaction(named).await.unwrap();
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
    let error = transcript
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

/// A session whose latest run, for "the task", made two calls, c1 and c2, after an earlier
/// question: the IDs of the earlier question, of the task, and of the two calls.
async fn a_run_with_two_calls() -> (Transcript, [String; 4]) {
    let mut transcript = Transcript::in_memory(header());
    let earlier = transcript
        .append_message(Message::user("an earlier question"))
        .await
        .unwrap();
    let task = transcript.append_message(Message::user("the task")).await.unwrap();
    let [call, result] = exchange("c1");
    let first_call = transcript.append_message(call).await.unwrap();
    transcript.append_message(result).await.unwrap();
    let [call, result] = exchange("c2");
    let second_call = transcript.append_message(call).await.unwrap();
    transcript.append_message(result).await.unwrap();
    (transcript, [earlier, task, first_call, second_call])
}

#[tokio::test]
async fn a_compaction_inside_a_run_keeps_the_input_of_that_run_before_its_summary() {
    let (mut transcript, [earlier, task, first_call, kept]) = a_run_with_two_calls().await;

    // Only the latest input before the boundary can be kept.
    let wrong = CompactionPlan {
        kept_input_entry_id: Some(earlier.clone()),
        ..plan(&transcript, &kept)
    };
    let error = transcript.begin_compaction(wrong).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("is not the latest input before its boundary"),
        "{error:#}"
    );

    let inside = CompactionPlan {
        kept_input_entry_id: Some(task.clone()),
        ..plan(&transcript, &kept)
    };
    let started = transcript.begin_compaction(inside).await.unwrap();
    transcript
        .append_compaction(&started, summary(&first_call), Usage::default())
        .await
        .unwrap();

    let texts: Vec<String> = transcript
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
    let lines: Vec<serde_json::Value> = transcript
        .entries()
        .iter()
        .map(|entry| serde_json::to_value(entry).unwrap())
        .collect();
    let recorded = lines.iter().find(|line| line["type"] == "compaction_started").unwrap();
    assert_eq!(recorded["keptInputEntryId"], task);
}

#[tokio::test]
async fn the_kept_tail_never_splits_an_assistant_tool_result_exchange() {
    let mut transcript = Transcript::in_memory(header());
    let old = transcript.append_message(Message::user("old evidence")).await.unwrap();
    let mut assistant = AssistantMessage::failure("faux", "m", StopReason::ToolUse, "");
    assistant.error_message = None;
    assistant.content = vec![ContentBlock::ToolCall(ToolCall {
        id: "c".into(),
        name: "read_file".into(),
        arguments: json!({"path":"x"}),
        arguments_error: None,
    })];
    let call = transcript.append_message(Message::Assistant(assistant)).await.unwrap();
    let result = transcript
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
    assert!(transcript.begin_compaction(plan(&transcript, &result)).await.is_err());
    let started = transcript.begin_compaction(plan(&transcript, &call)).await.unwrap();
    transcript
        .append_compaction(&started, summary(&old), Usage::default())
        .await
        .unwrap();
    let messages = transcript.messages();
    assert!(matches!(&messages[1], Message::Assistant(assistant) if assistant.tool_calls()[0].id == "c"));
    assert!(matches!(&messages[2], Message::ToolResult(result) if result.tool_call_id == "c"));
}
