use std::fs::OpenOptions;
use std::io::Write;
use std::sync::mpsc;

use async_trait::async_trait;
use futures::FutureExt;

use super::test_support::{assistant, header, texts};
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
    let mut store = SessionStore::in_memory(header());
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
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let mut store = SessionStore::create(&path, header()).unwrap();
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
    let leaf = store.leaf.clone();
    drop(store);
    // Written by another program, or by hand: appending through the store refuses it.
    let edit = Entry::ContextEdit(ContextEditEntry {
        id: "edit".into(),
        parent_id: leaf,
        timestamp: 0,
        target_id: system,
        replacement: None,
    });
    let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(file, "{}", serde_json::to_string(&edit).unwrap()).unwrap();
    drop(file);

    let error = SessionStore::open(&path).err().expect("the edit must be refused");

    assert!(format!("{error:#}").contains("system message"), "{error:#}");
}

#[tokio::test]
async fn context_edits_omit_or_replace_and_latest_wins() {
    let mut store = SessionStore::in_memory(header());
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
    let mut store = SessionStore::with_writer(header(), Box::new(FailingWriter));
    assert!(store.append_message(Message::user("q")).await.is_err());
    assert!(store.entries().is_empty());
    assert_eq!(store.leaf_id(), None);
}

#[tokio::test]
async fn a_log_receives_numbered_records_that_load_into_the_same_session() {
    let (sender, records) = mpsc::channel();
    let mut store = SessionStore::with_log(header(), Box::new(ChannelLog(sender)));
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
    let reloaded = SessionStore::load(loaded, Box::new(ChannelLog(sender))).unwrap();
    assert_eq!(reloaded.entries(), store.entries());
    assert_eq!(texts(&reloaded), ["edited", "a"]);
    assert_eq!(reloaded.leaf_id(), store.leaf_id());
}

#[tokio::test]
#[expect(clippy::too_many_lines, reason = "one entry of every kind")]
async fn records_carry_the_serialized_type_of_every_entry_kind() {
    let (sender, records) = mpsc::channel();
    let mut store = SessionStore::with_log(header(), Box::new(ChannelLog(sender)));
    let first = store.append_message(Message::user("old")).await.unwrap();
    store.append_message(assistant("a")).await.unwrap();
    let kept = store.append_message(Message::user("recent")).await.unwrap();
    let ids: Vec<String> = store.projection().into_iter().map(|item| item.entry_id).collect();
    let plan = || CompactionPlan {
        first_kept_entry_id: kept.clone(),
        source_entry_ids: ids[..2].to_vec(),
        request: SummaryRequest {
            provider: "p".into(),
            model: "m".into(),
            instructions: "summarize".into(),
            max_tokens: 10,
        },
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
        .record_request(&request, "p", serde_json::json!({}), RequestSource::Projection)
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
    let mut store = SessionStore::with_log(header(), Box::new(Unresponsive));
    store.append_timeout = Duration::from_millis(10);

    let error = store.append_message(Message::user("q")).await.unwrap_err();

    assert!(error.to_string().contains("did not answer within 10ms"), "{error}");
    assert!(store.entries().is_empty());
    store.log = Some(Box::new(JsonlFile::with_writer(Box::new(std::io::sink()))));
    let later = store.append_message(Message::user("later")).await.unwrap_err();
    assert!(later.to_string().contains("outcome is unknown"), "{later}");
}

#[tokio::test]
async fn why_writes_stopped_can_be_told_apart() {
    assert_eq!(SessionStore::in_memory(header()).writes_stopped(), None);

    let mut failed = SessionStore::with_writer(header(), Box::new(FailingWriter));
    failed.append_message(Message::user("q")).await.unwrap_err();
    assert_eq!(
        failed.writes_stopped(),
        Some(&WritesStopped::Failed("disk full".into()))
    );

    let mut silent = SessionStore::with_log(header(), Box::new(Unresponsive));
    silent.append_timeout = Duration::from_millis(10);
    silent.append_message(Message::user("q")).await.unwrap_err();
    assert_eq!(
        silent.writes_stopped(),
        Some(&WritesStopped::TimedOut(Duration::from_millis(10)))
    );

    let mut dropped = SessionStore::with_log(header(), Box::new(Unresponsive));
    assert!(dropped.append_message(Message::user("q")).now_or_never().is_none());
    assert_eq!(dropped.writes_stopped(), Some(&WritesStopped::Interrupted));

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    drop(SessionStore::create(&path, header()).unwrap());
    assert_eq!(
        SessionStore::read(&path).unwrap().writes_stopped(),
        Some(&WritesStopped::ReadOnly)
    );
}

#[tokio::test]
async fn a_file_write_that_blocks_times_out_without_stalling_the_runtime() {
    /// Answers every write late, like a session volume that stopped answering; bounded,
    /// so that a store that waits for it still ends.
    struct Stuck;
    impl std::io::Write for Stuck {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            std::thread::sleep(Duration::from_secs(2));
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut store = SessionStore::with_writer(header(), Box::new(Stuck));
    store.append_timeout = Duration::from_millis(50);
    let started = std::time::Instant::now();

    let error = store.append_message(Message::user("q")).await.unwrap_err();

    assert!(error.to_string().contains("did not answer within 50ms"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
}

#[tokio::test]
async fn an_append_dropped_while_waiting_stops_later_writes() {
    let mut store = SessionStore::with_log(header(), Box::new(Unresponsive));

    // Poll once: the log has the record but has not answered, so its outcome is unknown.
    assert!(store.append_message(Message::user("q")).now_or_never().is_none());

    assert!(store.entries().is_empty());
    store.log = Some(Box::new(JsonlFile::with_writer(Box::new(std::io::sink()))));
    let later = store.append_message(Message::user("later")).await.unwrap_err();
    assert!(later.to_string().contains("outcome is unknown"), "{later}");
}

/// Keeps `keep` bytes of the first write and then fails once, like a disk that fills up
/// in the middle of a record and is freed again. Later writes succeed.
struct PartialWriteOnce {
    file: std::fs::File,
    keep: usize,
    calls: usize,
}

impl Write for PartialWriteOnce {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.calls += 1;
        match self.calls {
            1 => {
                let part = &bytes[..self.keep.min(bytes.len())];
                self.file.write_all(part)?;
                Ok(part.len())
            }
            2 => Err(std::io::Error::other("disk full")),
            _ => self.file.write(bytes),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

#[tokio::test]
async fn a_partial_write_stops_later_writes_and_leaves_a_repairable_tail() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let mut store = SessionStore::create(&path, header()).unwrap();
    store.append_message(Message::user("kept")).await.unwrap();
    drop(store);
    let file = OpenOptions::new().append(true).open(&path).unwrap();
    let writer = PartialWriteOnce {
        file,
        keep: 10,
        calls: 0,
    };
    let mut store = SessionStore::with_writer(header(), Box::new(writer));

    assert!(store.append_message(Message::user("cut short")).await.is_err());
    // A run keeps emitting messages until it observes its cancellation.
    let later = store.append_message(Message::user("after the failure")).await;
    assert!(later.is_err(), "an entry was written after a failed write");
    drop(store);

    let reopened = SessionStore::open(&path).unwrap();
    assert_eq!(reopened.incomplete_tail_bytes(), 10);
    assert_eq!(texts(&reopened), ["kept"]);
}

/// The message for a session of `version`, which is not the current one.
fn unsupported(version: u32) -> String {
    format!("session format version {version} is not supported; only version {SESSION_VERSION} can be read or written")
}

#[test]
fn sessions_of_an_older_or_newer_version_are_refused_and_left_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    for version in [1, SESSION_VERSION - 1, SESSION_VERSION + 1] {
        let path = dir.path().join(format!("v{version}.jsonl"));
        let header =
            serde_json::json!({"type":"session","version":version,"id":"h","timestamp":1,"cwd":"/","model":"m"});
        let message =
            serde_json::json!({"type":"message","id":"a","parentId":null,"timestamp":2,"message":Message::user("hi")});
        let content = format!("{header}\n{message}\n");
        std::fs::write(&path, &content).unwrap();

        let opened = SessionStore::open(&path).err().unwrap().to_string();
        let read = SessionStore::read(&path).err().unwrap().to_string();

        let expected = format!("{}: {}", path.display(), unsupported(version));
        assert_eq!(opened, expected);
        assert_eq!(read, expected);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
    }
}

#[test]
fn a_session_of_another_version_is_not_created() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let older = SessionHeader {
        version: SESSION_VERSION - 1,
        ..header()
    };

    let error = SessionStore::create(&path, older).err().unwrap().to_string();

    assert_eq!(error, unsupported(SESSION_VERSION - 1));
    assert!(!path.exists());
}

#[tokio::test]
async fn context_edits_cannot_target_system_messages() {
    let mut store = SessionStore::in_memory(header());
    let system = Message::System(crate::ai::SystemMessage {
        sections: vec![],
        timestamp: 0,
    });
    let id = store.append_message(system).await.unwrap();
    assert!(store.append_context_edit(&id, None).await.is_err());
}

#[tokio::test]
async fn branches_select_a_path_and_edits_are_branch_local() {
    let mut store = SessionStore::in_memory(header());
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
