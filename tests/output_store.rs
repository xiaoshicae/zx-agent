//! Persistent tool output, redaction order, paging and filesystem boundaries.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;
use tokio_util::sync::CancellationToken;

use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::{Message, ToolCall};
use zx_agent::builtin::output_store::{Config, OutputStore, ReadOutputTool, SpillOutputs, TOOL_NAME};
use zx_agent::builtin::redact::RedactSecrets;
use zx_agent::builtin::tools::ReadFileTool;
use zx_agent::events::RunOutcome;
use zx_agent::hooks::{Hook, HookChain, HookContext};
use zx_agent::tools::{Tool, ToolContext, ToolOutput};
use zx_agent::transcript::Transcript;

use common::{RunnerSetup, config, header_in};

fn settings() -> Config {
    Config {
        threshold_bytes: Some(256),
        ..Config::default()
    }
}
fn call(name: &str) -> ToolCall {
    ToolCall {
        id: "c".into(),
        name: name.into(),
        arguments: json!({}),
        arguments_error: None,
    }
}
fn ctx(cwd: &Path) -> ToolContext {
    ToolContext::new(cwd.into(), CancellationToken::new())
}
/// The tools the command line exempts from storing: each pages its own output.
const PAGING_TOOLS: [&str; 3] = [TOOL_NAME, "read_skill", "read_file"];

fn parts(dir: &Path) -> (SpillOutputs, ReadOutputTool) {
    let store = OutputStore::new(dir, settings()).unwrap();
    SpillOutputs::with_reader(store, PAGING_TOOLS.map(String::from).to_vec())
}

/// The output ID that the footer of a stored result's page names.
fn output_id(page: &str) -> String {
    let (_, footer) = page
        .rsplit_once("\n[Use read_output with id=\"")
        .unwrap_or_else(|| panic!("no footer of a stored result in {page:?}"));
    footer.split('"').next().unwrap().to_string()
}

/// The file in which the store of `parts(dir)` keeps the result `id`.
fn stored_file(dir: &Path, id: &str) -> PathBuf {
    dir.join(".zx-agent/outputs").join(format!("{id}.txt"))
}

/// Pass `text` through `hook` as the result of a bash call. Returns what the hook leaves in
/// its place, and the ID of the stored result.
async fn spill(hook: &SpillOutputs, text: &str) -> (String, String) {
    let result = hook
        .after_tool(&call("bash"), ToolOutput::ok(text), &common::hook_context())
        .await
        .unwrap();
    let id = output_id(&result.content);
    (result.content, id)
}

#[tokio::test]
async fn files_are_lazy_persistent_and_contain_the_exact_bounded_tool_result() {
    let dir = tempfile::tempdir().unwrap();
    let (hook, _) = parts(dir.path());
    let root = dir.path().join(".zx-agent/outputs");
    assert!(!root.exists());
    let text = "already bounded output, including its truncation notice\n".repeat(100);
    let (_, id) = spill(&hook, &text).await;
    assert!(uuid::Uuid::parse_str(&id).is_ok());
    let path = stored_file(dir.path(), &id);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    drop(hook);
    assert!(
        path.is_file(),
        "results survive the writer's lifetime for session resume"
    );
    let (_, reader) = parts(dir.path());
    assert!(
        !reader
            .execute(json!({"id":id}), &ctx(dir.path()))
            .await
            .unwrap()
            .content
            .is_empty()
    );
}

#[tokio::test]
async fn exact_threshold_is_inline_and_error_and_terminate_flags_survive_spilling() {
    let dir = tempfile::tempdir().unwrap();
    let (hook, _) = parts(dir.path());
    let exact = ToolOutput::error("x".repeat(256));
    assert_eq!(
        hook.after_tool(&call("bash"), exact.clone(), &common::hook_context())
            .await
            .unwrap(),
        exact
    );
    let output = ToolOutput {
        content: "界".repeat(86),
        is_error: true,
        terminate: true,
        details: None,
        usage: None,
    };
    let spilled = hook
        .after_tool(&call("bash"), output.clone(), &common::hook_context())
        .await
        .unwrap();
    assert!(spilled.is_error && spilled.terminate);
    let stored = stored_file(dir.path(), &output_id(&spilled.content));
    assert_eq!(std::fs::read_to_string(stored).unwrap(), output.content);
}

#[tokio::test]
async fn a_stored_result_leaves_its_first_page_as_read_output_returns_it() {
    let dir = tempfile::tempdir().unwrap();
    let (hook, reader) = parts(dir.path());
    let text: String = (0..200).map(|line| format!("line {line}\n")).collect();

    let (left, id) = spill(&hook, &text).await;

    let (start, footer) = left.rsplit_once('\n').unwrap();
    assert!(!start.is_empty() && text.starts_with(start), "{left}");
    assert_eq!(
        footer,
        format!(
            "[Use read_output with id=\"{id}\", offset={} to continue.]",
            start.len()
        )
    );
    assert!(left.len() <= 256, "{} bytes", left.len());
    let first_page = reader.execute(json!({"id": id}), &ctx(dir.path())).await.unwrap();
    assert_eq!(left, first_page.content);
}

#[tokio::test]
async fn unicode_and_single_long_lines_are_paged_without_loss_or_oversized_pages() {
    let dir = tempfile::tempdir().unwrap();
    let (hook, reader) = parts(dir.path());
    let text = format!("{}\nend", "界🙂".repeat(1000));
    // The first page is the one the hook leaves; the tool reads the others.
    let (mut page, id) = spill(&hook, &text).await;
    let mut offset = 0;
    let mut joined = String::new();
    loop {
        assert!(page.len() <= 256);
        let Some((body, footer)) = page.split_once("\n[Use read_output") else {
            joined.push_str(&page);
            break;
        };
        joined.push_str(body);
        let next: usize = footer
            .split("offset=")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(next, offset + body.len());
        assert!(next > offset);
        offset = next;
        page = reader
            .execute(json!({"id":id,"offset":offset}), &ctx(dir.path()))
            .await
            .unwrap()
            .content;
    }
    assert_eq!(joined, text);
    assert!(
        reader
            .execute(json!({"id":id,"offset":1}), &ctx(dir.path()))
            .await
            .unwrap_err()
            .contains("UTF-8 character")
    );
    assert!(
        reader
            .execute(json!({"id":id,"offset":text.len()+1}), &ctx(dir.path()))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn output_ids_cannot_address_arbitrary_paths_and_schema_is_strict() {
    let dir = tempfile::tempdir().unwrap();
    let (_, reader) = parts(dir.path());
    for args in [
        json!({"id":"../escape"}),
        json!({"id":"/tmp/secret"}),
        json!({"id":"not-a-uuid"}),
        json!({"path":"x"}),
        json!({"id":uuid::Uuid::new_v4().to_string(),"offset":-1}),
        json!({"id":uuid::Uuid::new_v4().to_string(),"extra":true}),
    ] {
        assert!(reader.validate_arguments(&args).is_err(), "{args}");
        assert!(reader.execute(args, &ctx(dir.path())).await.is_err());
    }
    let missing = json!({"id":uuid::Uuid::new_v4().to_string()});
    assert!(reader.validate_arguments(&missing).is_ok());
    assert!(
        reader
            .execute(missing, &ctx(dir.path()))
            .await
            .unwrap_err()
            .contains("cannot read tool output")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_link_named_like_an_output_is_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    let (_, reader) = parts(dir.path());
    // What a command that can write the output directory could plant: a link, named like an
    // output, to a file elsewhere.
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("secret.txt");
    std::fs::write(&target, "kept outside the output directory").unwrap();
    let outputs = dir.path().join(".zx-agent/outputs");
    std::fs::create_dir_all(&outputs).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    std::os::unix::fs::symlink(&target, outputs.join(format!("{id}.txt"))).unwrap();

    let error = reader.execute(json!({"id": id}), &ctx(dir.path())).await.unwrap_err();

    assert!(error.contains("cannot read tool output"), "{error}");
    assert!(!error.contains("kept outside"), "{error}");
}

/// Store one result with `hook`, which creates the output directory of `dir`, then put a
/// link to another directory in its place, as a command that can write the working directory
/// could. Returns the other directory and the ID of the one output it holds.
#[cfg(unix)]
async fn swap_outputs_for_a_link(hook: &SpillOutputs, dir: &Path) -> (tempfile::TempDir, String) {
    hook.after_tool(&call("bash"), ToolOutput::ok("x".repeat(500)), &common::hook_context())
        .await
        .unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    std::fs::write(elsewhere.path().join(format!("{id}.txt")), "from another directory").unwrap();
    let outputs = dir.join(".zx-agent/outputs");
    std::fs::rename(&outputs, dir.join(".zx-agent/moved")).unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), &outputs).unwrap();
    (elsewhere, id)
}

#[cfg(unix)]
#[tokio::test]
async fn an_output_directory_swapped_for_a_link_is_not_read() {
    let dir = tempfile::tempdir().unwrap();
    let (hook, reader) = parts(dir.path());
    let (_elsewhere, id) = swap_outputs_for_a_link(&hook, dir.path()).await;

    let error = reader.execute(json!({"id": id}), &ctx(dir.path())).await.unwrap_err();

    assert!(error.contains("output directory"), "{error}");
    assert!(!error.contains("from another directory"), "{error}");
}

#[cfg(unix)]
#[tokio::test]
async fn nothing_is_stored_through_an_output_directory_swapped_for_a_link() {
    let dir = tempfile::tempdir().unwrap();
    let (hook, _) = parts(dir.path());
    let (elsewhere, _) = swap_outputs_for_a_link(&hook, dir.path()).await;

    let kept = hook
        .after_tool(&call("bash"), ToolOutput::ok("y".repeat(500)), &common::hook_context())
        .await
        .unwrap();

    assert!(kept.content.starts_with(&"y".repeat(500)), "{}", kept.content);
    assert!(kept.content.contains("[not stored: "), "{}", kept.content);
    // Only the file the test put there.
    assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn nothing_is_stored_through_a_link_planted_before_the_first_result() {
    // In place of the output directory, and of the directory above it.
    for linked in [".zx-agent/outputs", ".zx-agent"] {
        let dir = tempfile::tempdir().unwrap();
        let (hook, _) = parts(dir.path());
        let elsewhere = tempfile::tempdir().unwrap();
        let link = dir.path().join(linked);
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), &link).unwrap();

        let kept = hook
            .after_tool(&call("bash"), ToolOutput::ok("y".repeat(500)), &common::hook_context())
            .await
            .unwrap();

        assert!(kept.content.contains("[not stored: "), "{linked}: {}", kept.content);
        assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 0, "{linked}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn an_output_is_not_read_through_a_link_planted_before_the_first_read() {
    let dir = tempfile::tempdir().unwrap();
    let (_, reader) = parts(dir.path());
    let elsewhere = tempfile::tempdir().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    std::fs::write(elsewhere.path().join(format!("{id}.txt")), "from another directory").unwrap();
    std::fs::create_dir_all(dir.path().join(".zx-agent")).unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), dir.path().join(".zx-agent/outputs")).unwrap();

    let error = reader.execute(json!({"id": id}), &ctx(dir.path())).await.unwrap_err();

    assert!(error.contains("symbolic link"), "{error}");
    assert!(!error.contains("from another directory"), "{error}");
}

#[cfg(unix)]
#[tokio::test]
async fn an_output_directory_outside_the_working_directory_may_be_reached_through_a_link() {
    let dir = tempfile::tempdir().unwrap();
    // The user's own arrangement: commands that write the working directory cannot change it.
    let elsewhere = tempfile::tempdir().unwrap();
    let real = elsewhere.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = elsewhere.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let config = Config {
        directory: link.join("outputs"),
        ..settings()
    };
    let store = OutputStore::new(dir.path(), config).unwrap();
    let (hook, reader) = SpillOutputs::with_reader(store, Vec::new());

    let (left, id) = spill(&hook, &"z".repeat(500)).await;

    assert!(real.join("outputs").join(format!("{id}.txt")).is_file());
    let page = reader.execute(json!({"id": id}), &ctx(dir.path())).await.unwrap();
    assert_eq!(page.content, left);
}

#[tokio::test]
async fn file_caps_bound_both_writes_and_reads_and_invalid_utf8_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        max_file_bytes: 512,
        ..settings()
    };
    let store = OutputStore::new(dir.path(), config).unwrap();
    let (hook, reader) = SpillOutputs::with_reader(store, vec![]);
    // Over the cap nothing is written; the bounded result stays inline with the reason.
    let kept = hook
        .after_tool(&call("bash"), ToolOutput::ok("x".repeat(513)), &common::hook_context())
        .await
        .unwrap();
    assert!(kept.content.starts_with(&"x".repeat(513)));
    assert!(
        kept.content.contains("over the 512 byte file limit"),
        "{}",
        kept.content
    );
    assert!(!dir.path().join(".zx-agent").exists());
    let (_, id) = spill(&hook, &"x".repeat(512)).await;
    let args = json!({"id": id});
    assert!(reader.execute(args.clone(), &ctx(dir.path())).await.is_ok());
    let path = stored_file(dir.path(), &id);
    std::fs::write(&path, "x".repeat(513)).unwrap();
    assert!(
        reader
            .execute(args.clone(), &ctx(dir.path()))
            .await
            .unwrap_err()
            .contains("file limit")
    );
    std::fs::write(path, [0xff]).unwrap();
    assert!(
        reader
            .execute(args, &ctx(dir.path()))
            .await
            .unwrap_err()
            .contains("UTF-8")
    );
}

#[test]
fn a_result_not_stored_in_time_stays_inline() {
    // Storing runs on Tokio's blocking pool. A short limit alone races the write: Tokio polls
    // the write before its timer, which fires at a millisecond tick at the earliest, and a
    // fast disk can finish first. With the pool's only thread taken until the hook returns,
    // storing cannot finish in time.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let (release, released) = std::sync::mpsc::channel::<()>();
        let occupied = tokio::task::spawn_blocking(move || released.recv());
        let dir = tempfile::tempdir().unwrap();
        let (hook, _) = parts(dir.path());
        let hook = hook.write_timeout(std::time::Duration::ZERO);

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            hook.after_tool(&call("bash"), ToolOutput::ok("x".repeat(500)), &common::hook_context()),
        )
        .await
        .expect("the hook gives up storing at its limit")
        .unwrap();
        release.send(()).unwrap();
        occupied.await.unwrap().unwrap();

        assert!(output.content.starts_with(&"x".repeat(500)), "{}", output.content);
        assert!(
            output.content.contains("[not stored: storing took longer than 0ns"),
            "{}",
            output.content
        );
    });
}

#[tokio::test]
async fn paging_tools_are_exempt_and_disabled_spilling_is_a_noop() {
    let dir = tempfile::tempdir().unwrap();
    let (hook, _) = parts(dir.path());
    let large = ToolOutput::ok("x".repeat(500));
    for name in PAGING_TOOLS {
        assert_eq!(
            hook.after_tool(&call(name), large.clone(), &common::hook_context())
                .await
                .unwrap(),
            large
        );
    }
    let (disabled, _) = SpillOutputs::with_reader(
        OutputStore::new(
            dir.path(),
            Config {
                threshold_bytes: None,
                ..settings()
            },
        )
        .unwrap(),
        vec![],
    );
    assert_eq!(
        disabled
            .after_tool(&call("bash"), large.clone(), &common::hook_context())
            .await
            .unwrap(),
        large
    );
    assert!(!dir.path().join(".zx-agent").exists());
}

#[tokio::test]
async fn cancellation_skips_writes_and_reads() {
    let dir = tempfile::tempdir().unwrap();
    let (hook, reader) = parts(dir.path());
    let mut chain = HookChain::new();
    chain.push("hook", Arc::new(hook));
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(
        chain
            .after_tool(
                &call("bash"),
                ToolOutput::ok("x".repeat(500)),
                &HookContext::new("session".into(), dir.path().into(), cancel.clone())
            )
            .await
            .is_err()
    );
    assert!(!dir.path().join(".zx-agent").exists());
    let ctx = ToolContext::new(dir.path().into(), cancel);
    assert_eq!(
        reader
            .execute(json!({"id":uuid::Uuid::new_v4().to_string()}), &ctx)
            .await
            .unwrap_err(),
        "output read cancelled"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn new_directories_and_files_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let (hook, _) = parts(dir.path());
    let (_, id) = spill(&hook, &"private ".repeat(100)).await;
    let path = stored_file(dir.path(), &id);
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(
        std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(path.parent().unwrap().parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}

#[tokio::test]
async fn redaction_precedes_both_output_file_and_session_persistence() {
    let dir = tempfile::tempdir().unwrap();
    let secret = "synthetic-private-key";
    std::fs::write(dir.path().join("source.txt"), format!("value={secret}\n").repeat(100)).unwrap();
    let store = OutputStore::new(dir.path(), settings()).unwrap();
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c", "read_file", json!({"path":"source.txt"})),
        FauxStep::text("done"),
    ]));
    let (spill, reader) = SpillOutputs::with_reader(store, vec![TOOL_NAME.into()]);
    let mut config = config(
        provider.clone(),
        vec![Arc::new(ReadFileTool::default()), Arc::new(reader)],
    );
    config
        .hooks
        .push("redact_secrets", Arc::new(RedactSecrets::new([secret.into()])));
    config.hooks.push("spill_outputs", Arc::new(spill));
    let audit = dir.path().join("session.jsonl");
    let (runner, _) = RunnerSetup {
        session: common::create_session_file(&audit, header_in(dir.path()))
            .await
            .unwrap(),
        ..RunnerSetup::new(config)
    }
    .build();
    assert_eq!(runner.run("read it").await.outcome, RunOutcome::Completed);
    let requests = provider.requests();
    let Message::ToolResult(result) = &requests[1].messages[2] else {
        panic!("tool result");
    };
    let stored = std::fs::read_to_string(stored_file(dir.path(), &output_id(&result.content))).unwrap();
    assert!(!stored.contains(secret));
    assert!(stored.contains("[REDACTED]"));
    assert!(!result.content.contains(secret), "{}", result.content);
    assert!(!std::fs::read_to_string(&audit).unwrap().contains(secret));
    assert!(result.content.len() < stored.len());
}

#[tokio::test]
async fn a_created_output_directory_keeps_its_files_out_of_git() {
    let dir = tempfile::tempdir().unwrap();
    let (hook, _) = parts(dir.path());
    spill(&hook, &"z".repeat(1000)).await;
    let ignore = std::fs::read_to_string(dir.path().join(".zx-agent/outputs/.gitignore")).unwrap();
    assert!(ignore.lines().any(|line| line == "*"), "{ignore}");
}

#[tokio::test]
async fn a_storage_error_keeps_the_redacted_result_inline_and_the_run_continues() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("source.txt"), "x".repeat(1000)).unwrap();
    std::fs::write(dir.path().join("blocked"), "not a directory").unwrap();
    let store = OutputStore::new(
        dir.path(),
        Config {
            directory: PathBuf::from("blocked/outputs"),
            ..settings()
        },
    )
    .unwrap();
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c", "read_file", json!({"path":"source.txt"})),
        FauxStep::text("must not execute"),
    ]));
    let (spill, reader) = SpillOutputs::with_reader(store, vec![]);
    let mut config = config(
        provider.clone(),
        vec![Arc::new(ReadFileTool::default()), Arc::new(reader)],
    );
    config.hooks.push("spill_outputs", Arc::new(spill));
    let (runner, _) = RunnerSetup {
        session: Transcript::in_memory(header_in(dir.path())),
        ..RunnerSetup::new(config)
    }
    .build();
    let report = runner.run("go").await;
    // Storing is an optimisation: the hook runs after redaction, so a storage failure
    // keeps the already redacted, bounded result inline instead of hiding it.
    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(provider.requests().len(), 2);
    let requests = provider.requests();
    let Message::ToolResult(result) = &requests[1].messages[2] else {
        panic!("tool result");
    };
    assert!(!result.is_error);
    assert!(result.content.starts_with(&"x".repeat(1000)));
    assert!(result.content.contains("cannot create output directory"));
    assert!(!result.content.contains("Tool output withheld"));
}

#[test]
fn invalid_storage_budgets_are_rejected_at_startup() {
    for config in [
        Config {
            threshold_bytes: Some(1),
            ..settings()
        },
        Config {
            max_file_bytes: 0,
            ..settings()
        },
        Config {
            max_file_bytes: 1024 * 1024 + 1,
            ..settings()
        },
        Config {
            directory: PathBuf::new(),
            ..settings()
        },
    ] {
        assert!(config.validate().is_err());
    }
}
