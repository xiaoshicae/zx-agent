//! The file tools on this machine: write_file creates and replaces files, edit_file replaces
//! exact text, both report what they changed, and changes to one file in one batch are applied
//! one after another.

mod common;

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::task::Poll;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::builtin::executor::{CommandOutput, Executor, LocalExecutor, ShellCommand};
use zx_agent::builtin::tools::{EditFileTool, FileMutationQueue, WriteFileTool, builtin_tools};
use zx_agent::runner::{RunOutcome, RunReport};
use zx_agent::tools::{Tool, ToolContext, ToolOutput};
use zx_agent::transcript::Transcript;

use common::{RunnerSetup, config, header_in};

fn ctx(dir: &Path) -> ToolContext {
    ToolContext::new(dir.to_path_buf(), CancellationToken::new())
}

async fn write(dir: &Path, args: Value) -> Result<ToolOutput, String> {
    WriteFileTool::default().execute(args, &ctx(dir)).await
}

async fn edit(dir: &Path, args: Value) -> Result<ToolOutput, String> {
    EditFileTool::default().execute(args, &ctx(dir)).await
}

#[tokio::test]
async fn write_file_creates_missing_directories_and_replaces_an_existing_file() {
    let dir = tempfile::tempdir().unwrap();

    let created = write(dir.path(), json!({"path": "a/b/new.txt", "content": "first\n"}))
        .await
        .unwrap();
    let replaced = write(dir.path(), json!({"path": "a/b/new.txt", "content": "second"}))
        .await
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(dir.path().join("a/b/new.txt")).unwrap(),
        "second"
    );
    assert_eq!(created.content, "Wrote 6 bytes to a/b/new.txt.");
    assert!(!created.is_error);
    let details = replaced.details.unwrap();
    assert_eq!(details["bytes"], 6);
    assert!(details["path"].as_str().unwrap().ends_with("a/b/new.txt"));
}

#[tokio::test]
async fn write_file_refuses_a_directory() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();

    let error = write(dir.path(), json!({"path": "sub", "content": "x"}))
        .await
        .unwrap_err();

    assert!(error.contains("is a directory"), "{error}");
    assert!(dir.path().join("sub").is_dir());
}

#[tokio::test]
async fn edit_file_replaces_text_keeps_the_bom_and_line_endings_and_reports_a_patch() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config.ini");
    std::fs::write(&file, "\u{feff}[server]\r\nport = 80\r\nhost = local\r\n").unwrap();

    let output = edit(
        dir.path(),
        json!({"path": "config.ini", "edits": [{"oldText": "port = 80", "newText": "port = 8080"}]}),
    )
    .await
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "\u{feff}[server]\r\nport = 8080\r\nhost = local\r\n"
    );
    assert_eq!(output.content, "Replaced 1 block in config.ini.");
    let details = output.details.unwrap();
    assert_eq!(details["firstChangedLine"], 2);
    assert_eq!(
        details["patch"],
        "--- config.ini\n+++ config.ini\n@@ -1,3 +1,3 @@\n [server]\n-port = 80\n+port = 8080\n host = local\n"
    );
}

#[tokio::test]
async fn edit_file_applies_several_edits_in_one_call() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();

    let output = edit(
        dir.path(),
        json!({"path": "a.rs", "edits": [
            {"oldText": "fn c() {}", "newText": "fn c2() {}"},
            {"oldText": "fn a() {}", "newText": "fn a2() {}"}
        ]}),
    )
    .await
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.rs")).unwrap(),
        "fn a2() {}\nfn b() {}\nfn c2() {}\n"
    );
    assert_eq!(output.content, "Replaced 2 blocks in a.rs.");
}

#[tokio::test]
async fn a_failed_edit_leaves_the_file_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f.txt");
    let original = "same\nsame\nother\n";
    std::fs::write(&file, original).unwrap();
    for (edits, expected) in [
        (json!([{"oldText": "missing", "newText": "x"}]), "could not find"),
        (json!([{"oldText": "same", "newText": "x"}]), "found 2 occurrences"),
        (
            json!([{"oldText": "other", "newText": "x"}, {"oldText": "missing", "newText": "y"}]),
            "could not find edits[1]",
        ),
    ] {
        let error = edit(dir.path(), json!({"path": "f.txt", "edits": edits}))
            .await
            .unwrap_err();
        assert!(error.contains(expected), "{expected}: {error}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
    }
}

#[tokio::test]
async fn edit_file_refuses_missing_binary_and_non_utf8_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("bin"), b"a\0b").unwrap();
    std::fs::write(dir.path().join("latin1"), b"caf\xe9\n").unwrap();
    for (path, expected) in [
        ("missing.txt", "cannot read"),
        ("bin", "looks like a binary file"),
        ("latin1", "is not UTF-8 text"),
    ] {
        let error = edit(
            dir.path(),
            json!({"path": path, "edits": [{"oldText": "a", "newText": "b"}]}),
        )
        .await
        .unwrap_err();
        assert!(error.contains(expected), "{path}: {error}");
    }
    assert_eq!(std::fs::read(dir.path().join("latin1")).unwrap(), b"caf\xe9\n");
}

#[tokio::test]
async fn cancelled_file_changes_change_nothing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "old\n").unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let ctx = ToolContext::new(dir.path().to_path_buf(), cancel);

    let edited = EditFileTool::default()
        .execute(
            json!({"path": "f.txt", "edits": [{"oldText": "old", "newText": "new"}]}),
            &ctx,
        )
        .await;
    let written = WriteFileTool::default()
        .execute(json!({"path": "g.txt", "content": "x"}), &ctx)
        .await;

    assert!(edited.unwrap_err().contains("cancelled"));
    assert!(written.unwrap_err().contains("cancelled"));
    assert_eq!(std::fs::read_to_string(dir.path().join("f.txt")).unwrap(), "old\n");
    assert!(!dir.path().join("g.txt").exists());
}

/// More than a file write hands to a blocking thread in one piece (tokio: 2 MiB), so that a
/// write driven by the calling future would take several steps.
const LARGE_BYTES: usize = 5 * 1024 * 1024;

/// Poll `call`, which changes `file`, until the file is no longer `untouched_bytes` long, the
/// sign that its write has begun; then drop the call, as the loop drops the calls of a
/// cancelled run.
async fn drop_once_the_write_has_begun(
    call: impl Future<Output = Result<ToolOutput, String>>,
    file: &Path,
    untouched_bytes: usize,
) {
    let mut call = std::pin::pin!(call);
    loop {
        if let Poll::Ready(result) = futures::poll!(call.as_mut()) {
            // The call ended before it could be dropped; what it wrote is checked all the same.
            result.unwrap();
            return;
        }
        if file_len(file) != Some(untouched_bytes) {
            return;
        }
        tokio::task::yield_now().await;
    }
}

/// The length of `file`, when it can be told.
fn file_len(file: &Path) -> Option<usize> {
    let meta = std::fs::metadata(file).ok()?;
    usize::try_from(meta.len()).ok()
}

/// Whether `file` comes to hold `expected` within ten seconds. Nothing announces the end of a
/// write whose call was dropped, so the file is looked at until it is complete.
async fn comes_to_hold(file: &Path, expected: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if file_len(file) == Some(expected.len()) && std::fs::read_to_string(file).is_ok_and(|text| text == expected) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn a_write_whose_call_is_dropped_half_way_still_writes_the_whole_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f.txt");
    std::fs::write(&file, "old\n").unwrap();
    let content = "a line of the new content\n".repeat(LARGE_BYTES / 26);
    let tool = WriteFileTool::default();
    let ctx = ctx(dir.path());

    let call = tool.execute(json!({"path": "f.txt", "content": content}), &ctx);
    drop_once_the_write_has_begun(call, &file, "old\n".len()).await;

    assert!(
        comes_to_hold(&file, &content).await,
        "the file was left with {:?} of {} bytes",
        file_len(&file),
        content.len()
    );
}

#[tokio::test]
async fn an_edit_whose_call_is_dropped_half_way_still_writes_the_whole_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f.txt");
    let original = format!("{}MARK\n", "a line of the file\n".repeat(LARGE_BYTES / 19));
    std::fs::write(&file, &original).unwrap();
    let changed = original.replace("MARK", "CHANGED");
    let tool = EditFileTool::default();
    let ctx = ctx(dir.path());

    let call = tool.execute(
        json!({"path": "f.txt", "edits": [{"oldText": "MARK", "newText": "CHANGED"}]}),
        &ctx,
    );
    drop_once_the_write_has_begun(call, &file, original.len()).await;

    assert!(
        comes_to_hold(&file, &changed).await,
        "the file was left with {:?} of {} bytes",
        file_len(&file),
        changed.len()
    );
}

/// This machine's file system, with each read slowed down: two calls that both read before
/// either writes would lose one change.
struct SlowReads;

#[async_trait]
impl Executor for SlowReads {
    async fn run(&self, command: &ShellCommand<'_>, cancel: &CancellationToken) -> Result<CommandOutput, String> {
        LocalExecutor::default().run(command, cancel).await
    }

    async fn read_file(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, String> {
        let bytes = LocalExecutor::default().read_file(path, max_bytes).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        bytes
    }

    async fn write_file(&self, path: &Path, contents: &[u8]) -> Result<(), String> {
        LocalExecutor::default().write_file(path, contents).await
    }
}

/// Run the prompt "edit" in a session that works in `cwd`, with the file tools on
/// `executor`.
async fn edit_in(cwd: &Path, provider: Arc<FauxProvider>, executor: &Arc<dyn Executor>) -> RunReport {
    let names = ["edit_file", "write_file"].map(String::from);
    let tools = builtin_tools(&names, Duration::from_secs(5), executor).unwrap();
    let (runner, _) = RunnerSetup {
        session: Transcript::in_memory(header_in(cwd)),
        ..RunnerSetup::new(config(provider, tools))
    }
    .build();
    runner.run("edit").await
}

#[tokio::test]
async fn two_edits_of_one_file_in_one_batch_both_take_effect() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "one\ntwo\n").unwrap();
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_calls(vec![
            (
                "c1",
                "edit_file",
                json!({"path": "f.txt", "edits": [{"oldText": "one", "newText": "ONE"}]}),
            ),
            (
                "c2",
                "edit_file",
                json!({"path": "f.txt", "edits": [{"oldText": "two", "newText": "TWO"}]}),
            ),
        ]),
        FauxStep::text("done"),
    ]));
    let executor: Arc<dyn Executor> = Arc::new(SlowReads);

    let result = edit_in(dir.path(), provider, &executor).await;

    assert_eq!(result.outcome, RunOutcome::Completed);
    assert_eq!(std::fs::read_to_string(dir.path().join("f.txt")).unwrap(), "ONE\nTWO\n");
}

#[tokio::test]
async fn edits_of_different_files_in_one_batch_do_not_wait_for_each_other() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["a.txt", "b.txt"] {
        std::fs::write(dir.path().join(name), "x\n").unwrap();
    }
    let queue = FileMutationQueue::default();
    // Holding the turn of a.txt does not delay a change to b.txt.
    let _held = queue.turn(&dir.path().join("a.txt")).await;
    let tool = EditFileTool {
        queue: Arc::new(queue),
        ..EditFileTool::default()
    };
    let other = tokio::time::timeout(
        Duration::from_secs(5),
        tool.execute(
            json!({"path": "b.txt", "edits": [{"oldText": "x", "newText": "y"}]}),
            &ctx(dir.path()),
        ),
    )
    .await;

    assert!(other.unwrap().is_ok());
    assert_eq!(std::fs::read_to_string(dir.path().join("b.txt")).unwrap(), "y\n");
}

#[tokio::test]
async fn old_argument_shapes_are_rewritten_before_the_checks() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "a\nb\n").unwrap();
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call(
            "c1",
            "edit_file",
            json!({"path": "f.txt", "oldText": "a", "newText": "A"}),
        ),
        FauxStep::tool_call(
            "c2",
            "edit_file",
            json!({"path": "f.txt", "edits": json!([{"oldText": "b", "newText": "B"}]).to_string()}),
        ),
        FauxStep::text("done"),
    ]));
    let executor: Arc<dyn Executor> = Arc::new(LocalExecutor::default());

    let result = edit_in(dir.path(), provider, &executor).await;

    assert_eq!(result.outcome, RunOutcome::Completed);
    assert_eq!(std::fs::read_to_string(dir.path().join("f.txt")).unwrap(), "A\nB\n");
}
