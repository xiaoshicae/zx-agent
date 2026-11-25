//! What a finished tool call shows in the client. A client renders a text block as Markdown,
//! which joins lines and reads `#`, `-` or `*` at a line's start as markup, so the output of
//! a built-in tool and a failure go into a fenced code block; a change to a file goes as the
//! protocol's diffs, which a client shows in its own diff view.

use std::path::Path;

use agent_client_protocol::v1::schema::{Content, ContentBlock, Diff, TextContent, ToolCallContent};
use serde_json::Value;
use zx_agent::ai::ToolResultMessage;
use zx_agent::builtin::tools::{BASH, FIND, GREP, LS, READ_FILE, READ_OUTPUT, WRITE_FILE};

use crate::render::patch_of;

/// The built-in tools whose result is output to show as it is, not prose.
const OUTPUT_TOOLS: [&str; 6] = [BASH, READ_FILE, GREP, FIND, LS, READ_OUTPUT];

/// The fewest backticks a Markdown code fence takes.
const MIN_FENCE: usize = 3;

/// What `result` of a call with `args` shows besides its images, made of the text the model
/// received and the result's details; a relative file path is taken against `cwd`.
///
/// A change that has a patch, an edit's or a write's over a text file, is one diff per hunk;
/// any other write is a diff of the new file. A failure and the output of a built-in tool
/// are a code block. The text of any other tool, and the text beside images, which
/// describes them, is left as it is.
pub(super) fn result_content(result: &ToolResultMessage, args: Option<&Value>, cwd: &Path) -> Vec<ToolCallContent> {
    let ToolResultMessage {
        tool_name: name,
        content,
        images,
        is_error,
        details,
        ..
    } = result;
    let name = name.as_str();
    let file = args
        .and_then(|args| args.get("path"))
        .and_then(Value::as_str)
        .map(|file| cwd.join(file));
    let mut shown = match patch_of(name, details.as_ref()) {
        Some(patch) => patch_content(patch, file.as_deref()),
        None if name == WRITE_FILE && !is_error => new_file(args, file.as_deref()),
        None => Vec::new(),
    };
    if !content.is_empty() {
        shown.push(text_content(&if *is_error {
            fenced(content, "")
        } else if OUTPUT_TOOLS.contains(&name) && images.is_empty() {
            fenced(content, &language(name, file.as_deref()))
        } else {
            content.clone()
        }));
    }
    shown
}

/// A change to `file` as one diff per hunk of its `patch`, or the patch as a code block when
/// it was cut or the file is not known.
fn patch_content(patch: &str, file: Option<&Path>) -> Vec<ToolCallContent> {
    match (file, hunks(patch)) {
        (Some(file), Some(hunks)) => hunks
            .into_iter()
            .map(|hunk| ToolCallContent::Diff(Diff::new(file, hunk.new).old_text(hunk.old)))
            .collect(),
        _ => vec![text_content(&fenced(patch, "diff"))],
    }
}

/// A write without a patch as a diff of the whole new file: the file is new, or what it
/// replaced could not be compared.
fn new_file(args: Option<&Value>, file: Option<&Path>) -> Vec<ToolCallContent> {
    let written = args.and_then(|args| args.get("content")).and_then(Value::as_str);
    match (file, written) {
        (Some(file), Some(written)) => vec![ToolCallContent::Diff(Diff::new(file, written))],
        _ => Vec::new(),
    }
}

/// `text` as a tool call shows it, which a client renders as Markdown.
pub(super) fn text_content(text: &str) -> ToolCallContent {
    ToolCallContent::Content(Content::new(ContentBlock::Text(TextContent::new(text))))
}

/// The language a client highlights the output of `name` in: a read file's extension when it
/// is plain letters and digits, else none.
fn language(name: &str, file: Option<&Path>) -> String {
    if name != READ_FILE {
        return String::new();
    }
    file.and_then(Path::extension)
        .and_then(|extension| extension.to_str())
        .filter(|extension| extension.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or_default()
        .to_string()
}

/// `text` as a fenced code block in `language`. The fence is longer than any run of
/// backticks in `text`, so no line of it ends the block.
fn fenced(text: &str, language: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat(MIN_FENCE.max(longest + 1));
    let text = text.strip_suffix('\n').unwrap_or(text);
    format!("{fence}{language}\n{text}\n{fence}")
}

/// One hunk of a patch: the lines it replaced and the lines it became.
#[derive(Debug, Default, PartialEq)]
struct Hunk {
    old: String,
    new: String,
}

/// The side of a hunk the last line went to, which `\ No newline at end of file` refers to.
#[derive(Clone, Copy)]
enum Side {
    Old,
    New,
    Both,
}

/// The hunks of a unified `patch`, or `None` when a line after the first hunk header is not
/// part of a hunk, as the note on a patch cut to its limit is not.
fn hunks(patch: &str) -> Option<Vec<Hunk>> {
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut last = Side::Both;
    for line in patch.lines().skip_while(|line| !line.starts_with("@@")) {
        if line.starts_with("@@") {
            hunks.push(Hunk::default());
            continue;
        }
        let hunk = hunks.last_mut()?;
        if line.starts_with('\\') {
            if matches!(last, Side::Old | Side::Both) {
                hunk.old.pop();
            }
            if matches!(last, Side::New | Side::Both) {
                hunk.new.pop();
            }
            continue;
        }
        let (side, text) = if let Some(text) = line.strip_prefix(' ') {
            (Side::Both, text)
        } else if let Some(text) = line.strip_prefix('-') {
            (Side::Old, text)
        } else {
            (Side::New, line.strip_prefix('+')?)
        };
        if matches!(side, Side::Old | Side::Both) {
            hunk.old.push_str(text);
            hunk.old.push('\n');
        }
        if matches!(side, Side::New | Side::Both) {
            hunk.new.push_str(text);
            hunk.new.push('\n');
        }
        last = side;
    }
    (!hunks.is_empty()).then_some(hunks)
}

#[cfg(test)]
mod tests {
    use zx_agent::builtin::tools::EDIT_FILE;

    use super::*;

    fn result(name: &str, content: &str, details: Option<Value>) -> ToolResultMessage {
        ToolResultMessage {
            tool_call_id: "c1".to_string(),
            tool_name: name.to_string(),
            content: content.to_string(),
            images: Vec::new(),
            is_error: false,
            timestamp: 0,
            duration_ms: None,
            details,
            usage: None,
        }
    }

    fn shown(result: &ToolResultMessage, args: Option<&Value>, cwd: &str) -> Value {
        serde_json::to_value(result_content(result, args, Path::new(cwd))).unwrap()
    }

    fn hunk(old: &str, new: &str) -> Hunk {
        Hunk {
            old: old.to_string(),
            new: new.to_string(),
        }
    }

    #[test]
    fn a_patch_splits_into_the_old_and_new_text_of_each_hunk() {
        let patch = "--- f\n+++ f\n@@ -2,3 +2,3 @@\n two\n-three\n+THREE\n four\n@@ -9,2 +9,1 @@\n nine\n-ten\n";
        assert_eq!(
            hunks(patch),
            Some(vec![
                hunk("two\nthree\nfour\n", "two\nTHREE\nfour\n"),
                hunk("nine\nten\n", "nine\n"),
            ])
        );
    }

    #[test]
    fn a_missing_final_newline_is_kept_on_its_side() {
        let patch =
            "--- f\n+++ f\n@@ -1,2 +1,2 @@\n a\n-d\n\\ No newline at end of file\n+D\n\\ No newline at end of file\n";
        assert_eq!(hunks(patch), Some(vec![hunk("a\nd", "a\nD")]));
        let patch = "@@ -1,1 +1,1 @@\n same\n\\ No newline at end of file\n";
        assert_eq!(hunks(patch), Some(vec![hunk("same", "same")]));
    }

    #[test]
    fn a_cut_patch_or_one_without_hunks_has_none() {
        assert_eq!(
            hunks("@@ -1,2 +1,2 @@\n a\n-b\n\n[patch truncated at 20 of 900 bytes]\n"),
            None
        );
        assert_eq!(hunks("--- f\n+++ f\n"), None);
    }

    #[test]
    fn an_edit_shows_a_diff_per_hunk_and_a_cut_patch_as_a_diff_block() {
        let args = serde_json::json!({"path": "src/a.rs", "edits": []});
        let patch = "--- a\n+++ a\n@@ -1,1 +1,1 @@\n-x\n+y\n";
        let edited = result(
            EDIT_FILE,
            "Replaced 1 block.",
            Some(serde_json::json!({"patch": patch})),
        );
        let content = shown(&edited, Some(&args), "/work");
        assert_eq!(content[0]["type"], "diff", "{content}");
        assert_eq!(content[0]["path"], "/work/src/a.rs");
        assert_eq!(content[0]["oldText"], "x\n");
        assert_eq!(content[0]["newText"], "y\n");
        assert_eq!(content[1]["content"]["text"], "Replaced 1 block.");

        let patch = "@@ -1,1 +1,1 @@\n-x\n\n[patch truncated at 9 of 90 bytes]\n";
        let edited = result(
            EDIT_FILE,
            "Replaced 1 block.",
            Some(serde_json::json!({"patch": patch})),
        );
        let content = shown(&edited, Some(&args), "/work");
        let block = content[0]["content"]["text"].as_str().unwrap();
        assert!(block.starts_with("```diff\n@@ -1,1"), "{block}");
        assert!(block.ends_with("bytes]\n```"), "{block}");
    }

    #[test]
    fn a_write_over_a_text_file_shows_its_patch_and_a_new_file_shows_whole() {
        let args = serde_json::json!({"path": "a.md", "content": "two\n"});
        let patch = "--- a.md\n+++ a.md\n@@ -1,1 +1,1 @@\n-one\n+two\n";
        let replaced = result(
            WRITE_FILE,
            "Wrote 4 bytes to a.md.",
            Some(serde_json::json!({"patch": patch})),
        );
        let content = shown(&replaced, Some(&args), "/work");
        assert_eq!(content[0]["oldText"], "one\n", "{content}");
        assert_eq!(content[0]["newText"], "two\n");
        assert_eq!(content[1]["content"]["text"], "Wrote 4 bytes to a.md.");

        let created = result(WRITE_FILE, "Wrote 4 bytes to a.md.", None);
        let content = shown(&created, Some(&args), "/work");
        assert_eq!(content[0]["type"], "diff", "{content}");
        assert!(content[0].get("oldText").is_none_or(Value::is_null), "{content}");
        assert_eq!(content[0]["newText"], "two\n");
    }

    #[test]
    fn a_failure_is_a_code_block_and_a_failed_write_shows_no_diff() {
        let mut failed = result(WRITE_FILE, "sub is a directory <parameter>", None);
        failed.is_error = true;
        let content = shown(&failed, Some(&serde_json::json!({"path": "sub", "content": "x"})), "");
        assert_eq!(
            content,
            serde_json::json!([{"type": "content", "content": {"type": "text", "text": "```\nsub is a directory <parameter>\n```"}}])
        );
    }

    #[test]
    fn output_is_fenced_longer_than_its_backticks_and_a_read_file_by_its_extension() {
        let content = shown(&result(BASH, "# not a heading\n- not a list\n", None), None, "");
        assert_eq!(content[0]["content"]["text"], "```\n# not a heading\n- not a list\n```");

        let read = result(READ_FILE, "let s = \"````\";", None);
        let content = shown(&read, Some(&serde_json::json!({"path": "lib.rs"})), "");
        assert_eq!(content[0]["content"]["text"], "`````rs\nlet s = \"````\";\n`````");

        let content = shown(
            &result(READ_FILE, "x", None),
            Some(&serde_json::json!({"path": "odd.r`s"})),
            "",
        );
        assert_eq!(content[0]["content"]["text"], "```\nx\n```");
    }

    #[test]
    fn the_text_of_another_tool_or_beside_images_is_left_as_markdown() {
        let content = shown(&result("prom__query", "**up**", None), None, "");
        assert_eq!(content[0]["content"]["text"], "**up**");

        let mut read = result(READ_FILE, "Read image file [image/png]", None);
        read.images.push(zx_agent::ai::Image {
            data: String::new(),
            mime_type: "image/png".to_string(),
        });
        let content = shown(&read, Some(&serde_json::json!({"path": "dot.png"})), "");
        assert_eq!(content[0]["content"]["text"], "Read image file [image/png]");
    }
}
