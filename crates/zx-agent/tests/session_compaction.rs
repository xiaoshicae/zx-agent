//! Append-only compaction format and branch projection.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::{Message, Request, SystemMessage, SystemPrompt, Usage, now_ms};
use zx_agent::events::RunOutcome;
use zx_agent::hooks::{ContextEdit, Hook, HookContext, HookResult};
use zx_agent::transcript::{
    CompactionPlan, CompactionSummary, Entry, RequestSource, SESSION_VERSION, SummarizedBy, SummaryFact,
    SummaryRequest, Transcript,
};

use zx_testing::{RunnerSetup, Seed, answer_reporting, config, header, summarize};

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

fn plan(store: &Transcript, first_kept: &str) -> CompactionPlan {
    let source_entry_ids = store
        .raw_projection()
        .into_iter()
        .take_while(|item| item.entry_id != first_kept)
        .map(|item| item.entry_id)
        .collect();
    CompactionPlan {
        first_kept_entry_id: first_kept.into(),
        kept_input_entry_id: None,
        source_entry_ids,
        by: SummarizedBy::Request(SummaryRequest {
            provider: "faux".into(),
            model: "m".into(),
            instructions: "Summarize".into(),
            max_tokens: 100,
        }),
        input_bytes: 1000,
    }
}

async fn seeded(store: &mut Transcript) -> (String, String) {
    let old = store.append_message(Message::user("old evidence")).await.unwrap();
    let kept = store.append_message(Message::user("latest input")).await.unwrap();
    (old, kept)
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
async fn a_compaction_ends_the_measurement_by_the_answers_before_it() {
    let mut store = Transcript::in_memory(header());
    let old = store.append_message(Message::user("old evidence")).await.unwrap();
    let kept = store.append_message(answer_reporting(900)).await.unwrap();
    let latest = Message::user("latest input");
    store.append_message(latest.clone()).await.unwrap();
    let (usage, rest) = store.measured_context();
    assert_eq!(usage.map(|usage| usage.input), Some(900));
    assert_eq!(rest, [latest]);

    let started = store.begin_compaction(plan(&store, &kept)).await.unwrap();
    store
        .append_compaction(&started, summary(&old), Usage::default())
        .await
        .unwrap();

    // The kept answer was sent the context from before the compaction.
    let (usage, rest) = store.measured_context();
    assert_eq!(usage, None);
    assert_eq!(rest, store.messages());
}

#[tokio::test]
async fn an_answer_recorded_after_a_compaction_measures_the_compacted_context() {
    let mut store = Transcript::in_memory(header());
    let (old, kept) = seeded(&mut store).await;
    let started = store.begin_compaction(plan(&store, &kept)).await.unwrap();
    store
        .append_compaction(&started, summary(&old), Usage::default())
        .await
        .unwrap();
    store.append_message(answer_reporting(300)).await.unwrap();
    let later = Message::user("next input");
    store.append_message(later.clone()).await.unwrap();

    let (usage, rest) = store.measured_context();

    assert_eq!(usage.map(|usage| usage.input), Some(300));
    assert_eq!(rest, [later]);
}

/// An old message, an answer that reported 900 prompt tokens, and a later input; returns the
/// three entry IDs.
async fn measured(store: &mut Transcript) -> [String; 3] {
    let earlier = store.append_message(Message::user("old evidence")).await.unwrap();
    let answer = store.append_message(answer_reporting(900)).await.unwrap();
    let later = store.append_message(Message::user("latest input")).await.unwrap();
    [earlier, answer, later]
}

#[tokio::test]
async fn an_edit_of_a_later_message_keeps_the_measurement_by_an_answer() {
    let mut store = Transcript::in_memory(header());
    let [_, _, later] = measured(&mut store).await;

    // What the answer was sent is as it was.
    store
        .append_context_edit(&later, Some("rewritten".into()))
        .await
        .unwrap();

    assert_eq!(store.measured_context().0.map(|usage| usage.input), Some(900));
}

#[tokio::test]
async fn an_edit_at_or_before_an_answer_ends_its_measurement() {
    for edited in [0, 1] {
        let mut store = Transcript::in_memory(header());
        let ids = measured(&mut store).await;

        store
            .append_context_edit(&ids[edited], Some("shorter".into()))
            .await
            .unwrap();

        assert_eq!(store.measured_context().0, None, "edited entry {edited}");
    }
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
async fn duplicate_ids_and_unknown_records_are_rejected_without_modifying_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut store = zx_testing::create_session_file(&path, header()).await.unwrap();
    let (id, _) = seeded(&mut store).await;
    drop(store);
    let prefix = std::fs::read_to_string(&path).unwrap();
    for bad in [
        json!({"type":"context_edit","id":id,"parentId":id,"timestamp":0,"targetId":id,"replacement":null}),
        // The upgrade record of formats before the current one.
        json!({"type":"format_upgrade","id":"upgrade","parentId":id,"timestamp":0,"version":SESSION_VERSION}),
    ] {
        let content = format!("{prefix}{bad}\n");
        std::fs::write(&path, &content).unwrap();
        assert!(zx_testing::open_session_file(&path).await.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
    }
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
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut store = zx_testing::create_session_file(&path, header()).await.unwrap();
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
    drop(store);

    let lines = zx_testing::read_lines(&path);
    let starts: Vec<_> = lines
        .iter()
        .filter(|line| line["type"] == "compaction_started")
        .collect();
    assert_eq!(starts[0]["request"]["maxTokens"], 100);
    assert!(starts[0].get("summarizer").is_none());
    assert_eq!(starts[1]["summarizer"], "notes");
    assert!(starts[1].get("request").is_none());
    // Both load again, each as it was written.
    let reopened = zx_testing::open_session_file(&path).await.unwrap();
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
    use zx_agent::ai::{AssistantMessage, ContentBlock, StopReason, ToolCall, ToolResultMessage};
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
async fn a_turn_with_two_calls() -> (Seed, [String; 4]) {
    let mut store = Seed::in_memory(header()).await;
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
async fn a_context_plan_that_ends_a_summary_still_edits_the_messages_it_named() {
    /// Revises the kept input, which ends the summary that read it, and trims c2's result.
    struct ReviseTheTaskAndTrimC2;
    #[async_trait]
    impl Hook for ReviseTheTaskAndTrimC2 {
        async fn transform_context(&self, messages: &[Message], _ctx: &HookContext) -> HookResult<Vec<ContextEdit>> {
            let task = messages
                .iter()
                .position(|message| matches!(message, Message::User { content, .. } if content == "the task"));
            let c2 = messages
                .iter()
                .position(|message| matches!(message, Message::ToolResult(result) if result.content == "value of c2"));
            Ok(match (task, c2) {
                (Some(task), Some(c2)) => vec![
                    ContextEdit::ReplaceText {
                        index: task,
                        text: "the task, revised".into(),
                    },
                    ContextEdit::ReplaceText {
                        index: c2,
                        text: "trimmed".into(),
                    },
                ],
                _ => Vec::new(),
            })
        }
    }
    let (mut store, [_, task, first_call, second_call]) = a_turn_with_two_calls().await;
    let inside = CompactionPlan {
        kept_input_entry_id: Some(task),
        ..plan(&store, &second_call)
    };
    let started = store.begin_compaction(inside).await.unwrap();
    store
        .append_compaction(&started, summary(&first_call), Usage::default())
        .await
        .unwrap();
    let provider = Arc::new(FauxProvider::new([FauxStep::text("done")]));
    let mut config = config(provider.clone(), vec![]);
    config.hooks.push("revise", Arc::new(ReviseTheTaskAndTrimC2));
    let (runner, _) = RunnerSetup {
        session: store.into(),
        ..RunnerSetup::new(config)
    }
    .build()
    .await;

    let report = runner.continue_run().await.unwrap();

    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    let sent = summarize(&provider.requests()[0].messages);
    assert!(sent.iter().any(|message| message == "tool:c2=trimmed"), "{sent:?}");
    assert!(sent.iter().any(|message| message == "tool:c1=value of c1"), "{sent:?}");
}

#[tokio::test]
async fn a_retained_boundary_never_splits_an_assistant_tool_result_exchange() {
    use zx_agent::ai::{AssistantMessage, ContentBlock, StopReason, ToolCall, ToolResultMessage};
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
