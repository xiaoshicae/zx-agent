use std::sync::mpsc;

use async_trait::async_trait;
use futures::FutureExt;

use super::test_support::{assistant, header, loaded, records_of, texts};
use super::*;
use crate::ai::{AssistantMessage, ContentBlock, StopReason, ToolCall, Usage};

/// Sends every record it stores to the test, which keeps the receiver.
struct ChannelLog(mpsc::Sender<(u64, String)>);

#[async_trait]
impl SessionLog for ChannelLog {
    async fn append(&mut self, record: &LogRecord<'_>) -> Result<()> {
        self.0.send((record.seq, record.json.to_string()))?;
        Ok(())
    }
}

/// Never answers, like a database that stopped responding.
struct Unresponsive;

#[async_trait]
impl SessionLog for Unresponsive {
    async fn append(&mut self, _record: &LogRecord<'_>) -> Result<()> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn a_text_replacement_cannot_erase_tool_calls() {
    let mut store = Transcript::in_memory(header());
    store.append_message(Message::user("q")).await.unwrap();
    let mut answer = AssistantMessage::failure("p", "m", StopReason::ToolUse, "");
    answer.error_message = None;
    answer.content = vec![ContentBlock::ToolCall(ToolCall {
        id: "c1".into(),
        name: "bash".into(),
        arguments: serde_json::json!({}),
        arguments_error: None,
    })];
    let target = store.append_message(Message::Assistant(answer)).await.unwrap();

    let error = store
        .append_context_edit(&target, Some("no calls".into()))
        .await
        .unwrap_err();

    assert!(error.to_string().contains("tool calls"), "{error}");
    store.append_context_edit(&target, None).await.unwrap();
}

#[tokio::test]
async fn opening_rejects_a_context_edit_of_a_system_message() {
    let mut store = Transcript::in_memory(header());
    let system = store
        .append_message(Message::System(crate::ai::SystemMessage {
            sections: vec![crate::ai::SectionChange {
                name: "preamble".into(),
                content: Some("rules".into()),
            }],
            timestamp: 0,
        }))
        .await
        .unwrap();
    store.append_message(Message::user("q")).await.unwrap();
    // Written by another program, or by hand: appending through the store refuses it.
    let edit = Entry::ContextEdit(ContextEditEntry {
        id: "edit".into(),
        parent_id: store.leaf.clone(),
        timestamp: 0,
        target_id: system,
        replacement: None,
    });
    let mut records = records_of(&store);
    records.push(serde_json::to_string(&edit).unwrap());

    let error = Transcript::snapshot(loaded(header(), records))
        .err()
        .expect("the edit must be refused");

    assert!(format!("{error:#}").contains("system message"), "{error:#}");
}

#[tokio::test]
async fn context_edits_omit_or_replace_and_latest_wins() {
    let mut store = Transcript::in_memory(header());
    store.append_message(Message::user("q")).await.unwrap();
    let failed = store.append_message(assistant("bad")).await.unwrap();
    let answer = store.append_message(assistant("good")).await.unwrap();
    store.append_context_edit(&failed, None).await.unwrap();
    assert_eq!(texts(&store), ["q", "good"]);
    store.append_context_edit(&answer, Some("first".into())).await.unwrap();
    store.append_context_edit(&answer, Some("second".into())).await.unwrap();
    assert_eq!(texts(&store), ["q", "second"]);
    assert_eq!(store.entries().len(), 6, "raw history keeps every entry");
}

#[tokio::test]
async fn a_failed_write_leaves_the_tree_unchanged() {
    let mut store = Transcript::with_writer(header(), Box::new(FailingWriter));
    assert!(store.append_message(Message::user("q")).await.is_err());
    assert!(store.entries().is_empty());
    assert_eq!(store.leaf_id(), None);
}

#[tokio::test]
async fn a_log_receives_numbered_records_that_load_into_the_same_session() {
    let (sender, records) = mpsc::channel();
    let mut store = Transcript::with_log(header(), Box::new(ChannelLog(sender)));
    let question = store.append_message(Message::user("q")).await.unwrap();
    store.append_message(assistant("a")).await.unwrap();
    store
        .append_context_edit(&question, Some("edited".into()))
        .await
        .unwrap();

    let received: Vec<(u64, String)> = records.try_iter().collect();
    let seqs: Vec<u64> = received.iter().map(|(seq, _)| *seq).collect();
    assert_eq!(seqs, [1, 2, 3]);
    let loaded = LoadedSession {
        header: store.header().clone(),
        records: received.into_iter().map(|(_, json)| json).collect(),
        incomplete_tail_bytes: 0,
    };
    let (sender, _records) = mpsc::channel();
    let reloaded = Transcript::load(loaded, Box::new(ChannelLog(sender))).unwrap();
    assert_eq!(reloaded.entries(), store.entries());
    assert_eq!(texts(&reloaded), ["edited", "a"]);
    assert_eq!(reloaded.leaf_id(), store.leaf_id());
}

#[tokio::test]
#[expect(clippy::too_many_lines, reason = "one entry of every kind")]
async fn records_carry_the_serialized_type_of_every_entry_kind() {
    let (sender, records) = mpsc::channel();
    let mut store = Transcript::with_log(header(), Box::new(ChannelLog(sender)));
    let first = store.append_message(Message::user("old")).await.unwrap();
    store.append_message(assistant("a")).await.unwrap();
    let kept = store.append_message(Message::user("recent")).await.unwrap();
    let ids: Vec<String> = store.projection().into_iter().map(|item| item.entry_id).collect();
    let plan = || CompactionPlan {
        first_kept_entry_id: kept.clone(),
        kept_input_entry_id: None,
        source_entry_ids: ids[..2].to_vec(),
        by: SummarizedBy::Request(SummaryRequest {
            provider: "p".into(),
            model: "m".into(),
            instructions: "summarize".into(),
            max_tokens: 10,
        }),
        input_bytes: 1,
    };
    let started = store.begin_compaction(plan()).await.unwrap();
    store
        .fail_compaction(&started, "invalid".into(), Usage::default(), None)
        .await
        .unwrap();
    let started = store.begin_compaction(plan()).await.unwrap();
    let summary = CompactionSummary {
        facts: vec![SummaryFact {
            text: "fact".into(),
            source_entry_ids: vec![first.clone()],
        }],
        inferences: vec![],
        plans: vec![],
    };
    store
        .append_compaction(&started, summary, Usage::default())
        .await
        .unwrap();
    let request = crate::ai::Request {
        cache_key: None,
        model: "m".into(),
        messages: store.messages(),
        tools: vec![],
        max_tokens: None,
        reasoning: None,
    };
    store
        .record_request(&request, "p", serde_json::json!({}), RequestSource::Task)
        .await
        .unwrap();
    let mut call = AssistantMessage::failure("p", "m", StopReason::ToolUse, "");
    call.error_message = None;
    let tool_call = ToolCall {
        id: "c1".into(),
        name: "bash".into(),
        arguments: serde_json::json!({}),
        arguments_error: None,
    };
    call.content = vec![ContentBlock::ToolCall(ToolCall {
        id: tool_call.id.clone(),
        name: tool_call.name.clone(),
        arguments: tool_call.arguments.clone(),
        arguments_error: None,
    })];
    store.append_message(Message::Assistant(call)).await.unwrap();
    store.append_context_edit(&first, None).await.unwrap();
    store
        .append_tool_call_started(&tool_call.id, &tool_call.name)
        .await
        .unwrap();
    store.append_custom_entry("state", serde_json::json!(1)).await.unwrap();
    store
        .append_custom_message(CustomMessage {
            custom_type: "note".into(),
            content: "remember".into(),
            display: true,
            details: None,
        })
        .await
        .unwrap();
    store
        .append_session_name(SessionName::new("checks").unwrap())
        .await
        .unwrap();

    let mut kinds: Vec<&str> = store.entries().iter().map(Entry::kind).collect();
    for ((_, json), entry) in records.try_iter().zip(store.entries()) {
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["type"], entry.kind());
        assert_eq!(value["timestamp"], entry.timestamp());
    }
    kinds.sort_unstable();
    kinds.dedup();
    assert_eq!(kinds.len(), 11, "every entry kind was checked: {kinds:?}");
}

#[tokio::test]
async fn an_append_that_times_out_stops_later_writes() {
    let mut store = Transcript::with_log(header(), Box::new(Unresponsive));
    store.append_timeout = Duration::from_millis(10);

    let error = store.append_message(Message::user("q")).await.unwrap_err();

    assert!(error.to_string().contains("did not answer within 10ms"), "{error}");
    assert!(store.entries().is_empty());
    store.log = Some(Box::new(WriterLog::new(Box::new(std::io::sink()))));
    let later = store.append_message(Message::user("later")).await.unwrap_err();
    assert!(later.to_string().contains("outcome is unknown"), "{later}");
}

#[tokio::test]
async fn a_later_append_says_why_writes_stopped() {
    /// Why `store` refuses the next append.
    async fn refusal(store: &mut Transcript) -> String {
        store
            .append_message(Message::user("later"))
            .await
            .unwrap_err()
            .to_string()
    }
    let mut writing = Transcript::in_memory(header());
    assert!(writing.append_message(Message::user("q")).await.is_ok());

    let mut failed = Transcript::with_writer(header(), Box::new(FailingWriter));
    failed.append_message(Message::user("q")).await.unwrap_err();
    let error = refusal(&mut failed).await;
    assert!(error.contains("an earlier write failed: disk full"), "{error}");

    let mut silent = Transcript::with_log(header(), Box::new(Unresponsive));
    silent.append_timeout = Duration::from_millis(10);
    silent.append_message(Message::user("q")).await.unwrap_err();
    let error = refusal(&mut silent).await;
    assert!(error.contains("did not finish within 10ms"), "{error}");

    let mut dropped = Transcript::with_log(header(), Box::new(Unresponsive));
    assert!(dropped.append_message(Message::user("q")).now_or_never().is_none());
    let error = refusal(&mut dropped).await;
    assert!(error.contains("interrupted before its log answered"), "{error}");

    let mut snapshot = Transcript::snapshot(loaded(header(), Vec::new())).unwrap();
    let error = refusal(&mut snapshot).await;
    assert!(error.contains("read as a snapshot, which is read-only"), "{error}");
}

#[tokio::test]
async fn an_append_dropped_while_waiting_stops_later_writes() {
    let mut store = Transcript::with_log(header(), Box::new(Unresponsive));

    // Poll once: the log has the record but has not answered, so its outcome is unknown.
    assert!(store.append_message(Message::user("q")).now_or_never().is_none());

    assert!(store.entries().is_empty());
    store.log = Some(Box::new(WriterLog::new(Box::new(std::io::sink()))));
    let later = store.append_message(Message::user("later")).await.unwrap_err();
    assert!(later.to_string().contains("outcome is unknown"), "{later}");
}

/// The message for a session of `version`, which is not the current one.
fn unsupported(version: u32) -> String {
    format!("session format version {version} is not supported; only version {SESSION_VERSION} can be read or written")
}

#[tokio::test]
async fn sessions_of_an_older_or_newer_version_are_refused() {
    for version in [1, SESSION_VERSION - 1, SESSION_VERSION + 1] {
        let other = SessionHeader { version, ..header() };
        let message =
            serde_json::json!({"type":"message","id":"a","parentId":null,"timestamp":2,"message":Message::user("hi")});
        let records = vec![message.to_string()];

        let log = Box::new(WriterLog::new(Box::new(std::io::sink())));
        let opened = Transcript::load(loaded(other.clone(), records.clone()), log);
        let read = Transcript::snapshot(loaded(other, records));

        assert_eq!(opened.err().unwrap().to_string(), unsupported(version));
        assert_eq!(read.err().unwrap().to_string(), unsupported(version));
    }
}

#[tokio::test]
async fn a_session_of_another_version_is_not_created() {
    let older = SessionHeader {
        version: SESSION_VERSION - 1,
        ..header()
    };

    let error = MemoryStorage::default().create(&older).await.err().unwrap().to_string();

    assert_eq!(error, unsupported(SESSION_VERSION - 1));
}

#[tokio::test]
async fn an_actor_is_recorded_with_a_prompt_and_with_no_other_message() {
    let storage = MemoryStorage::default();
    let header = header();
    let mut store = Transcript::with_log(header.clone(), storage.create(&header).await.unwrap());
    store
        .append_message_from(Message::user("first"), Some("alice".into()))
        .await
        .unwrap();
    store.append_message(Message::user("second")).await.unwrap();
    let recorded = store.entries().len();

    let error = store
        .append_message_from(assistant("answer"), Some("alice".into()))
        .await
        .unwrap_err();

    assert!(
        format!("{error:#}").contains("only a person's input names an actor"),
        "{error:#}"
    );
    assert_eq!(store.entries().len(), recorded, "nothing is written");
    drop(store);
    let stored = storage.read(&header.id).await.unwrap();
    let lines: Vec<serde_json::Value> = stored
        .records
        .iter()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines[0]["actor"], "alice");
    assert!(lines[1].get("actor").is_none(), "{}", lines[1]);
    let reopened = Transcript::snapshot(stored).unwrap();
    assert_eq!(reopened.latest_actor(), None, "the last prompt names nobody");
}

#[tokio::test]
async fn the_latest_actor_is_that_of_the_last_prompt_of_the_branch() {
    let mut store = Transcript::in_memory(header());
    assert_eq!(store.latest_actor(), None);
    let first = store
        .append_message_from(Message::user("one"), Some("alice".into()))
        .await
        .unwrap();
    store.append_message(assistant("answer")).await.unwrap();
    assert_eq!(store.latest_actor(), Some("alice"), "an answer is not an input");

    store
        .append_message_from(Message::user("two"), Some("bob".into()))
        .await
        .unwrap();
    assert_eq!(store.latest_actor(), Some("bob"));

    store.branch(&first).unwrap();
    assert_eq!(store.latest_actor(), Some("alice"), "Bob spoke on another branch");
}

#[tokio::test]
async fn context_edits_cannot_target_system_messages() {
    let mut store = Transcript::in_memory(header());
    let system = Message::System(crate::ai::SystemMessage {
        sections: vec![],
        timestamp: 0,
    });
    let id = store.append_message(system).await.unwrap();
    assert!(store.append_context_edit(&id, None).await.is_err());
}

#[tokio::test]
async fn branches_select_a_path_and_edits_are_branch_local() {
    let mut store = Transcript::in_memory(header());
    let q = store.append_message(Message::user("q")).await.unwrap();
    let a = store.append_message(assistant("a")).await.unwrap();
    store.append_context_edit(&a, None).await.unwrap();
    store.append_message(Message::user("old branch")).await.unwrap();
    assert_eq!(texts(&store), ["q", "old branch"]);

    store.branch(&a).unwrap();
    store.append_message(Message::user("new branch")).await.unwrap();
    // The edit lives on the old branch, so `a` is visible again here.
    assert_eq!(texts(&store), ["q", "a", "new branch"]);
    assert!(store.append_context_edit("missing", None).await.is_err());
    store.branch(&q).unwrap();
    assert!(
        store.append_context_edit(&a, None).await.is_err(),
        "target not on the active branch"
    );
}
