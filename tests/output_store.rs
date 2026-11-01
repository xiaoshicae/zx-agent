//! Persistent tool output, redaction order, paging and filesystem boundaries.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use zx_agent::agent::{HookChain, Hooks, Tool, ToolContext, ToolOutput};
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::{Message, ToolCall};
use zx_agent::extensions::RedactSecrets;
use zx_agent::output_store::{Config, OutputStore, ReadOutputTool, SpillOutputs, TOOL_NAME};
use zx_agent::runner::RunOutcome;
use zx_agent::session::SessionStore;
use zx_agent::tools::ReadFileTool;

use common::{RunnerSetup, config, header};

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
    ToolContext {
        cwd: cwd.into(),
        cancel: CancellationToken::new(),
    }
}
fn parts(dir: &Path) -> (SpillOutputs, ReadOutputTool) {
    let store = OutputStore::new(dir, settings()).unwrap();
    (
        SpillOutputs::new(store.clone(), vec![TOOL_NAME.into(), "read_skill".into()]),
        ReadOutputTool::new(store),
    )
}
async fn spill(hook: &SpillOutputs, text: &str) -> Value {
    let result = hook.after_tool(&call("read_file"), ToolOutput::ok(text)).await.unwrap();
    serde_json::from_str(&result.content).unwrap()
}

#[tokio::test]
async fn files_are_lazy_persistent_and_contain_the_exact_bounded_tool_result() {
    let dir = tempfile::tempdir().unwrap();
    let (hook, _) = parts(dir.path());
    let root = dir.path().join(".zx-agent/outputs");
    assert!(!root.exists());
    let text = "already bounded output, including its truncation notice\n".repeat(100);
    let reference = spill(&hook, &text).await;
    let id = reference["outputId"].as_str().unwrap();
    assert!(uuid::Uuid::parse_str(id).is_ok());
    let path = PathBuf::from(reference["path"].as_str().unwrap());
    assert_eq!(path.parent(), Some(root.as_path()));
    assert_eq!(reference["bytes"], text.len());
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
    assert_eq!(hook.after_tool(&call("bash"), exact.clone()).await.unwrap(), exact);
    let output = ToolOutput {
        content: "界".repeat(86),
        is_error: true,
        terminate: true,
    };
    let spilled = hook.after_tool(&call("bash"), output.clone()).await.unwrap();
    assert!(spilled.is_error && spilled.terminate);
    let reference: Value = serde_json::from_str(&spilled.content).unwrap();
    assert_eq!(
        std::fs::read_to_string(reference["path"].as_str().unwrap()).unwrap(),
        output.content
    );
}

#[tokio::test]
async fn unicode_and_single_long_lines_are_paged_without_loss_or_oversized_pages() {
    let dir = tempfile::tempdir().unwrap();
    let (hook, reader) = parts(dir.path());
    let text = format!("{}\nend", "界🙂".repeat(1000));
    let reference = spill(&hook, &text).await;
    let id = reference["outputId"].as_str().unwrap();
    let mut offset = 0;
    let mut joined = String::new();
    loop {
        let output = reader
            .execute(json!({"id":id,"offset":offset}), &ctx(dir.path()))
            .await
            .unwrap();
        assert!(output.content.len() <= 256);
        if let Some((body, footer)) = output.content.split_once("\n[Use read_output") {
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
        } else {
            joined.push_str(&output.content);
            break;
        }
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

#[tokio::test]
async fn file_caps_bound_both_writes_and_reads_and_invalid_utf8_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        max_file_bytes: 512,
        ..settings()
    };
    let store = OutputStore::new(dir.path(), config).unwrap();
    let hook = SpillOutputs::new(store.clone(), vec![]);
    let reader = ReadOutputTool::new(store);
    // Over the cap nothing is written; the bounded result stays inline with the reason.
    let kept = hook
        .after_tool(&call("bash"), ToolOutput::ok("x".repeat(513)))
        .await
        .unwrap();
    assert!(kept.content.starts_with(&"x".repeat(513)));
    assert!(kept.content.contains("over toolOutput.maxFileBytes"));
    assert!(!dir.path().join(".zx-agent").exists());
    let reference = spill(&hook, &"x".repeat(512)).await;
    let args = json!({"id":reference["outputId"]});
    assert!(reader.execute(args.clone(), &ctx(dir.path())).await.is_ok());
    let path = reference["path"].as_str().unwrap();
    std::fs::write(path, "x".repeat(513)).unwrap();
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

#[tokio::test]
async fn paging_tools_are_exempt_and_disabled_spilling_is_a_noop() {
    let dir = tempfile::tempdir().unwrap();
    let (hook, _) = parts(dir.path());
    let large = ToolOutput::ok("x".repeat(500));
    for name in [TOOL_NAME, "read_skill"] {
        assert_eq!(hook.after_tool(&call(name), large.clone()).await.unwrap(), large);
    }
    let disabled = SpillOutputs::new(
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
    assert_eq!(disabled.after_tool(&call("bash"), large.clone()).await.unwrap(), large);
    assert!(!dir.path().join(".zx-agent").exists());
}

#[tokio::test]
async fn cancellation_skips_writes_and_reads() {
    let dir = tempfile::tempdir().unwrap();
    let (hook, reader) = parts(dir.path());
    let mut chain = HookChain::new();
    chain.push(hook);
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(
        chain
            .after_tool(&call("bash"), ToolOutput::ok("x".repeat(500)), &cancel)
            .await
            .is_err()
    );
    assert!(!dir.path().join(".zx-agent").exists());
    let ctx = ToolContext {
        cwd: dir.path().into(),
        cancel,
    };
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
async fn new_directories_and_files_are_private_and_uuid_file_symlinks_are_rejected() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = tempfile::tempdir().unwrap();
    let (hook, reader) = parts(dir.path());
    let reference = spill(&hook, &"private ".repeat(100)).await;
    let path = PathBuf::from(reference["path"].as_str().unwrap());
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
    let other = dir.path().join("outside.txt");
    std::fs::write(&other, "outside").unwrap();
    std::fs::remove_file(&path).unwrap();
    symlink(&other, &path).unwrap();
    assert!(
        reader
            .execute(json!({"id":reference["outputId"]}), &ctx(dir.path()))
            .await
            .unwrap_err()
            .contains("cannot read tool output")
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
    let mut config = config(
        provider.clone(),
        vec![
            Arc::new(ReadFileTool::default()),
            Arc::new(ReadOutputTool::new(store.clone())),
        ],
    );
    config.spec.cwd = dir.path().into();
    config.hooks.push(RedactSecrets::new([secret.into()]));
    config.hooks.push(SpillOutputs::new(store, vec![TOOL_NAME.into()]));
    let audit = dir.path().join("session.jsonl");
    let (mut runner, _) = RunnerSetup {
        session: SessionStore::create(&audit, header()).unwrap(),
        ..RunnerSetup::new(config)
    }
    .build();
    assert_eq!(
        runner.run("read it", &CancellationToken::new()).await.outcome,
        RunOutcome::Completed
    );
    let requests = provider.requests();
    let Message::ToolResult(result) = &requests[1].messages[2] else {
        panic!("tool result");
    };
    let reference: Value = serde_json::from_str(&result.content).unwrap();
    let stored = std::fs::read_to_string(reference["path"].as_str().unwrap()).unwrap();
    assert!(!stored.contains(secret));
    assert!(stored.contains("[REDACTED]"));
    assert!(!std::fs::read_to_string(&audit).unwrap().contains(secret));
    assert!(result.content.len() < stored.len());
}

#[cfg(unix)]
#[tokio::test]
async fn a_linked_default_directory_outside_the_working_directory_is_never_written() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("repository");
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::create_dir(&elsewhere).unwrap();
    // A repository can commit this link; following it would write outside the project.
    std::os::unix::fs::symlink("../elsewhere", cwd.join(".zx-agent")).unwrap();
    let (hook, _) = parts(&cwd);
    let result = hook
        .after_tool(&call("read_file"), ToolOutput::ok("x".repeat(1000)))
        .await
        .unwrap();
    assert!(result.content.starts_with(&"x".repeat(1000)), "{}", result.content);
    assert!(
        result.content.contains("outside the working directory"),
        "{}",
        result.content
    );
    assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
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
    let mut config = config(
        provider.clone(),
        vec![
            Arc::new(ReadFileTool::default()),
            Arc::new(ReadOutputTool::new(store.clone())),
        ],
    );
    config.spec.cwd = dir.path().into();
    config.hooks.push(SpillOutputs::new(store, vec![]));
    let (mut runner, _) = RunnerSetup::new(config).build();
    let report = runner.run("go", &CancellationToken::new()).await;
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
