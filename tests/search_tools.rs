//! The grep, find and ls tools on this machine. They use ripgrep when it is installed and
//! POSIX grep and find otherwise; the assertions hold for both.

#![cfg(unix)]

use std::path::Path;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use zx_agent::agent::{Effects, Tool, ToolContext, ToolOutput};
use zx_agent::tools::{FindTool, GrepTool, LsTool};

fn ctx(dir: &Path) -> ToolContext {
    ToolContext::new(dir.to_path_buf(), CancellationToken::new())
}

/// A small project: sources at two depths, a dotfile, and a version control directory.
fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src/nested")).unwrap();
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {\n    println!(\"Hello\");\n}\n").unwrap();
    std::fs::write(root.join("src/nested/util.rs"), "pub fn hello() {}\n// a.b and axb\n").unwrap();
    std::fs::write(root.join("README.md"), "hello world\n").unwrap();
    std::fs::write(root.join(".env.example"), "KEY=hello\n").unwrap();
    std::fs::write(root.join(".git/config"), "hello from git\n").unwrap();
    dir
}

async fn grep(dir: &Path, args: Value) -> Result<ToolOutput, String> {
    GrepTool::default().execute(args, &ctx(dir)).await
}

async fn find(dir: &Path, args: Value) -> Result<ToolOutput, String> {
    FindTool::default().execute(args, &ctx(dir)).await
}

async fn ls(dir: &Path, args: Value) -> Result<ToolOutput, String> {
    LsTool::default().execute(args, &ctx(dir)).await
}

/// The result lines without the notes in brackets.
fn lines(output: &ToolOutput) -> Vec<String> {
    output
        .content
        .lines()
        .filter(|line| !line.starts_with('['))
        .map(|line| line.trim_start_matches("./").to_string())
        .collect()
}

#[test]
fn the_search_tools_are_read_only() {
    assert_eq!(GrepTool::default().effects(), Effects::ReadOnly);
    assert_eq!(FindTool::default().effects(), Effects::ReadOnly);
    assert_eq!(LsTool::default().effects(), Effects::ReadOnly);
}

#[tokio::test]
async fn grep_returns_matching_lines_with_paths_and_line_numbers() {
    let dir = project();

    let output = grep(dir.path(), json!({"pattern": "hello", "path": "src"}))
        .await
        .unwrap();

    assert_eq!(lines(&output), ["src/nested/util.rs:1:pub fn hello() {}"]);
    let engine = output.details.unwrap()["engine"].as_str().unwrap().to_string();
    assert!(["rg", "grep"].contains(&engine.as_str()), "{engine}");
    if engine == "grep" {
        assert!(
            output.content.contains("ripgrep is not installed"),
            "{}",
            output.content
        );
    }
}

#[tokio::test]
async fn grep_options_change_what_matches() {
    let dir = project();
    let insensitive = grep(
        dir.path(),
        json!({"pattern": "HELLO", "path": "src", "ignoreCase": true}),
    )
    .await
    .unwrap();
    assert_eq!(lines(&insensitive).len(), 2, "{}", insensitive.content);
    let regex = grep(dir.path(), json!({"pattern": "a.b", "path": "src/nested"}))
        .await
        .unwrap();
    assert_eq!(lines(&regex), ["src/nested/util.rs:2:// a.b and axb"]);
    let literal = grep(
        dir.path(),
        json!({"pattern": "a.b", "path": "src/nested/util.rs", "literal": true}),
    )
    .await
    .unwrap();
    assert_eq!(lines(&literal), ["src/nested/util.rs:2:// a.b and axb"]);
    let glob = grep(dir.path(), json!({"pattern": "hello", "glob": "*.md"}))
        .await
        .unwrap();
    assert_eq!(lines(&glob), ["README.md:1:hello world"]);
}

#[tokio::test]
async fn grep_includes_dotfiles_and_skips_version_control() {
    let dir = project();

    let output = grep(dir.path(), json!({"pattern": "hello"})).await.unwrap();

    let mut found = lines(&output);
    found.sort();
    assert_eq!(
        found,
        [
            ".env.example:1:KEY=hello",
            "README.md:1:hello world",
            "src/nested/util.rs:1:pub fn hello() {}"
        ]
    );
}

#[tokio::test]
async fn grep_reports_no_matches_and_errors() {
    let dir = project();
    let none = grep(dir.path(), json!({"pattern": "absent-text"})).await.unwrap();
    assert_eq!(none.content, "No matches found.");
    let missing = grep(dir.path(), json!({"pattern": "x", "path": "no/such/dir"}))
        .await
        .unwrap_err();
    assert!(!missing.is_empty());
    let invalid = grep(dir.path(), json!({"pattern": "(unclosed"})).await.unwrap_err();
    assert!(!invalid.is_empty());
}

#[tokio::test]
async fn grep_shows_at_most_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("many.txt"), "match\n".repeat(10)).unwrap();

    let output = grep(dir.path(), json!({"pattern": "match", "limit": 3})).await.unwrap();

    assert_eq!(lines(&output).len(), 3, "{}", output.content);
    assert!(output.content.contains("more matches exist"), "{}", output.content);
}

#[tokio::test]
async fn grep_notes_more_matches_when_just_over_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("many.txt"), "match\n".repeat(5)).unwrap();

    let output = grep(dir.path(), json!({"pattern": "match", "limit": 3})).await.unwrap();

    assert_eq!(lines(&output).len(), 3, "{}", output.content);
    assert!(output.content.contains("more matches exist"), "{}", output.content);
}

#[tokio::test]
async fn hostile_patterns_and_paths_are_searched_for_not_run() {
    let dir = project();
    for value in [
        "$(touch pwned)",
        "`touch pwned`",
        "'; touch pwned; '",
        "-e x; touch pwned",
    ] {
        // Whether these searches succeed does not matter, only that nothing ran.
        let _ = grep(dir.path(), json!({"pattern": value, "path": value})).await;
        let _ = find(dir.path(), json!({"pattern": value, "path": value})).await;
        let _ = ls(dir.path(), json!({"path": value})).await;
        assert!(!dir.path().join("pwned").exists(), "{value:?} ran a command");
    }
}

#[tokio::test]
async fn find_matches_names_at_any_depth_and_paths_with_a_slash() {
    let dir = project();

    let by_name = find(dir.path(), json!({"pattern": "*.rs"})).await.unwrap();
    let by_path = find(dir.path(), json!({"pattern": "src/**/*.rs"})).await.unwrap();
    let in_directory = find(dir.path(), json!({"pattern": "*.rs", "path": "src/nested"}))
        .await
        .unwrap();

    assert_eq!(lines(&by_name), ["src/main.rs", "src/nested/util.rs"]);
    assert_eq!(lines(&by_path), ["src/main.rs", "src/nested/util.rs"]);
    assert_eq!(lines(&in_directory), ["util.rs"]);
}

#[tokio::test]
async fn find_includes_dotfiles_skips_version_control_and_shows_at_most_the_limit() {
    let dir = project();
    let all = find(dir.path(), json!({"pattern": "*"})).await.unwrap();
    assert_eq!(
        lines(&all),
        [".env.example", "README.md", "src/main.rs", "src/nested/util.rs"]
    );
    let limited = find(dir.path(), json!({"pattern": "*", "limit": 2})).await.unwrap();
    assert_eq!(lines(&limited), [".env.example", "README.md"]);
    assert!(limited.content.contains("more files match"), "{}", limited.content);
    let none = find(dir.path(), json!({"pattern": "*.go"})).await.unwrap();
    assert_eq!(none.content, "No files found.");
    let missing = find(dir.path(), json!({"pattern": "*", "path": "no/such/dir"}))
        .await
        .unwrap_err();
    assert!(!missing.is_empty());
}

#[tokio::test]
async fn ls_lists_entries_with_directories_marked() {
    let dir = project();

    let output = ls(dir.path(), json!({})).await.unwrap();

    assert_eq!(lines(&output), [".env.example", ".git/", "README.md", "src/"]);
    let limited = ls(dir.path(), json!({"path": "src", "limit": 1})).await.unwrap();
    assert_eq!(lines(&limited), ["main.rs"]);
    assert!(limited.content.contains("more entries exist"), "{}", limited.content);
    std::fs::create_dir(dir.path().join("empty")).unwrap();
    assert_eq!(
        ls(dir.path(), json!({"path": "empty"})).await.unwrap().content,
        "(empty directory)"
    );
    assert!(ls(dir.path(), json!({"path": "missing"})).await.is_err());
}

#[tokio::test]
async fn long_matching_lines_are_cut() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("min.js"), format!("needle{}\n", "x".repeat(5000))).unwrap();

    let output = grep(dir.path(), json!({"pattern": "needle"})).await.unwrap();

    let line = output.content.lines().next().unwrap();
    assert!(line.ends_with(" [line cut]"), "{line}");
    assert!(line.len() < 600, "{}", line.len());
}

#[tokio::test]
async fn a_cancelled_search_reports_the_cancellation() {
    let dir = project();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let ctx = ToolContext::new(dir.path().to_path_buf(), cancel);

    let error = GrepTool::default()
        .execute(json!({"pattern": "hello"}), &ctx)
        .await
        .unwrap_err();

    assert!(error.contains("cancelled"), "{error}");
}
