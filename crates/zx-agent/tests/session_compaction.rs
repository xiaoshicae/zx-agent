//! Compaction records in a stored session: a session file refuses records it cannot load, and
//! a run continues a session that a compaction left, hooks and all. The records and the
//! projection themselves are tested in `zx-core` (`transcript/compaction/tests.rs`).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::{AssistantMessage, Message, Usage, now_ms};
use zx_agent::events::RunOutcome;
use zx_agent::hooks::{ContextEdit, Hook, HookContext, HookResult};
use zx_agent::transcript::{
    CompactionEntry, CompactionStartedEntry, CompactionSummary, Entry, SESSION_VERSION, SummarizedBy, SummaryFact,
    SummaryRequest,
};

use zx_testing::{RunnerSetup, Seed, config, header, summarize};

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

async fn seeded(seed: &mut Seed) -> (String, String) {
    let old = seed.append_message(Message::user("old evidence")).await.unwrap();
    let kept = seed.append_message(Message::user("latest input")).await.unwrap();
    (old, kept)
}

#[tokio::test]
async fn duplicate_ids_and_unknown_records_are_rejected_without_modifying_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut seed = zx_testing::create_session_file(&path, header()).await.unwrap();
    let (id, _) = seeded(&mut seed).await;
    drop(seed);
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

/// An answer that calls `read_file` as `id`, and that call's result.
fn exchange(id: &str) -> [Message; 2] {
    use zx_agent::ai::{StopReason, ToolResultMessage};
    let assistant = AssistantMessage::new(
        "faux",
        "m",
        StopReason::ToolUse,
        vec![zx_testing::tool_call_block(id, "read_file", json!({"path": "x"}))],
    );
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

/// A session whose latest prompt, "the task", ran two calls, c1 and c2, after an earlier
/// question: the IDs of the earlier question, of the task, and of the two calls.
async fn a_prompt_with_two_calls() -> (Seed, [String; 4]) {
    let mut seed = Seed::in_memory(header()).await;
    let earlier = seed.append_message(Message::user("an earlier question")).await.unwrap();
    let task = seed.append_message(Message::user("the task")).await.unwrap();
    let [call, result] = exchange("c1");
    let first_call = seed.append_message(call).await.unwrap();
    seed.append_message(result).await.unwrap();
    let [call, result] = exchange("c2");
    let second_call = seed.append_message(call).await.unwrap();
    seed.append_message(result).await.unwrap();
    (seed, [earlier, task, first_call, second_call])
}

/// Record, as a run leaves them, a compaction of `seed` that keeps from `first_kept`, with
/// `kept_input` before its summary, and whose summary cites `cited`.
async fn record_compaction(seed: &mut Seed, first_kept: &str, kept_input: &str, cited: &str) {
    let source_entry_ids = seed
        .raw_projection()
        .into_iter()
        .take_while(|item| item.entry_id != first_kept)
        .map(|item| item.entry_id)
        .collect();
    let started = Entry::CompactionStarted(CompactionStartedEntry {
        id: "compaction-started".into(),
        parent_id: seed.leaf_id().map(String::from),
        timestamp: now_ms(),
        first_kept_entry_id: first_kept.into(),
        kept_input_entry_id: Some(kept_input.into()),
        source_entry_ids,
        by: SummarizedBy::Request(SummaryRequest {
            provider: "faux".into(),
            model: "m".into(),
            instructions: "Summarize".into(),
            max_tokens: 100,
        }),
        input_bytes: 1000,
    });
    seed.append_record(started).await.unwrap();
    let done = Entry::Compaction(CompactionEntry {
        id: "compaction".into(),
        parent_id: Some("compaction-started".into()),
        timestamp: now_ms(),
        compaction_id: "compaction-started".into(),
        first_kept_entry_id: first_kept.into(),
        summary: summary(cited),
        usage: Usage::default(),
    });
    seed.append_record(done).await.unwrap();
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
    let (mut transcript, [_, task, first_call, second_call]) = a_prompt_with_two_calls().await;
    record_compaction(&mut transcript, &second_call, &task, &first_call).await;
    assert!(transcript.latest_compaction().is_some(), "the summary stands in for c1");
    let provider = Arc::new(FauxProvider::new([FauxStep::text("done")]));
    let mut config = config(provider.clone(), vec![]);
    config.hooks.push("revise", Arc::new(ReviseTheTaskAndTrimC2));
    let (runner, _) = RunnerSetup {
        session: transcript.into(),
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
