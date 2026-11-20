//! File coordination across cancellation and independently built sessions.
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use zx_agent::builtin::executor::{CommandOutput, Executor, LocalExecutor, ShellCommand};
use zx_agent::builtin::tools::{FileMutationQueue, WriteFileTool, executor_tools};
use zx_agent::tools::{Tool, ToolContext};
use zx_testing::WAIT;

/// Holds the first operation after it has started; cancellation cannot interrupt its write.
#[derive(Default)]
struct HeldFile {
    started: Arc<Notify>,
    finished: Arc<Notify>,
    release: CancellationToken,
    reads: AtomicUsize,
}

#[async_trait]
impl Executor for HeldFile {
    async fn run(&self, command: &ShellCommand<'_>, cancel: &CancellationToken) -> Result<CommandOutput, String> {
        LocalExecutor::default().run(command, cancel).await
    }

    async fn read_file(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, String> {
        let bytes = LocalExecutor::default().read_file(path, max_bytes).await?;
        if self.reads.fetch_add(1, Ordering::SeqCst) == 0 {
            self.started.notify_one();
            self.release.cancelled().await;
        }
        Ok(bytes)
    }

    async fn write_file(&self, path: &Path, contents: &[u8]) -> Result<(), String> {
        let (path, contents) = (path.to_path_buf(), contents.to_vec());
        let (started, finished, release) = (self.started.clone(), self.finished.clone(), self.release.clone());
        tokio::spawn(async move {
            if contents == b"old" {
                started.notify_one();
                release.cancelled().await;
            }
            let result = LocalExecutor::default().write_file(&path, &contents).await;
            if contents == b"old" {
                finished.notify_one();
            }
            result
        })
        .await
        .unwrap()
    }
}

fn context(cwd: PathBuf) -> ToolContext {
    ToolContext::new(cwd, CancellationToken::new())
}

#[tokio::test]
async fn dropping_a_write_keeps_its_file_locked_until_the_write_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let executor = Arc::new(HeldFile::default());
    let tool = Arc::new(WriteFileTool {
        executor: executor.clone(),
        ..WriteFileTool::default()
    });
    let first = Arc::clone(&tool);
    let cwd = dir.path().to_path_buf();
    let task = tokio::spawn(async move {
        first
            .execute(json!({"path": "f", "content": "old"}), &context(cwd))
            .await
    });
    executor.started.notified().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());

    let ctx = context(dir.path().into());
    let next = tool.execute(json!({"path": "f", "content": "new"}), &ctx);
    tokio::pin!(next);
    let early = tokio::time::timeout(Duration::from_millis(50), &mut next).await;
    executor.release.cancel();
    let waited = early.is_err();
    let changed = match early {
        Ok(changed) => changed,
        Err(_) => tokio::time::timeout(WAIT, &mut next).await.unwrap(),
    };
    changed.unwrap();
    executor.finished.notified().await;
    assert_eq!(std::fs::read_to_string(dir.path().join("f")).unwrap(), "new");
    assert!(waited, "a newer write ran before the old write finished");
}

#[tokio::test]
async fn independent_toolsets_coordinate_edits_of_the_same_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "a=0\nb=0\n").unwrap();
    let held = Arc::new(HeldFile::default());
    let executor: Arc<dyn Executor> = held.clone();
    // Two sessions' tools, which share the queue of changes to each file as their host has
    // them share it.
    let queue = Arc::new(FileMutationQueue::default());
    let tools = || {
        executor_tools(&["edit_file".into()], WAIT, &executor, &queue)
            .unwrap()
            .remove(0)
    };
    let (first, second) = (tools(), tools());
    let ctx = context(dir.path().into());
    let edit = first.execute(json!({"path":"f", "edits":[{"oldText":"a=0", "newText":"a=1"}]}), &ctx);
    tokio::pin!(edit);
    tokio::select! {
        () = held.started.notified() => {},
        result = &mut edit => panic!("the first edit ended before its read was released: {result:?}"),
    }
    let later = second.execute(json!({"path":"f", "edits":[{"oldText":"b=0", "newText":"b=1"}]}), &ctx);
    tokio::pin!(later);
    let early = tokio::time::timeout(Duration::from_millis(50), &mut later).await;
    held.release.cancel();
    edit.await.unwrap();
    match early {
        Ok(result) => result.unwrap(),
        Err(_) => tokio::time::timeout(WAIT, later).await.unwrap().unwrap(),
    };
    assert_eq!(std::fs::read_to_string(dir.path().join("f")).unwrap(), "a=1\nb=1\n");
}
