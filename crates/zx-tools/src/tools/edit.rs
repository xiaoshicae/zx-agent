//! `edit_file` tool: exact text replacements in one file (pi: coding-agent/src/core/tools/edit.ts).

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::edit_text::{
    Edit, apply_edits, line_ending, normalize_to_lf, restore_line_endings, split_bom, unified_patch,
};
use super::mutation::FileMutationQueue;
use super::read::resolve;
use crate::agent::{ExecutionMode, Tool, ToolContext, ToolOutput};
use crate::ai::ToolDef;
use crate::executor::{Executor, LocalExecutor};

/// The most bytes of the patch in a result's details. Details over the loop's limit
/// (`MAX_DETAILS_BYTES`, 16 KiB) are dropped whole, so a long patch is cut to leave room for
/// the other fields.
const MAX_PATCH_BYTES: usize = 12 * 1024;

/// Replaces exact, unique, non-overlapping pieces of text in one file.
pub struct EditFileTool {
    /// Larger files are refused.
    pub max_file_bytes: u64,
    /// Where files are read and written. Defaults to this machine.
    pub executor: Arc<dyn Executor>,
    /// Shared with `write_file`, so that changes to one file happen one at a time.
    pub queue: Arc<FileMutationQueue>,
}

impl Default for EditFileTool {
    fn default() -> Self {
        Self {
            max_file_bytes: 10 * 1024 * 1024,
            executor: Arc::new(LocalExecutor),
            queue: Arc::default(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    path: String,
    edits: Vec<Edit>,
}

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        super::EDIT_FILE
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: super::EDIT_FILE.to_string(),
            description: "Edit one file by exact text replacement. Every edits[].oldText must match a unique region of \
                          the original file, including whitespace and line breaks, and must not overlap another edit; \
                          all edits are matched against the original, not one after another. Change several places of \
                          one file in one call. If two changes touch the same or nearby lines, merge them into one edit. \
                          Keep oldText as short as possible while unique. Relative paths resolve against the working \
                          directory."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path" },
                    "edits": {
                        "type": "array",
                        "minItems": 1,
                        "description": "Targeted replacements, matched against the original file",
                        "items": {
                            "type": "object",
                            "properties": {
                                "oldText": { "type": "string", "description": "Exact text to replace; unique in the file" },
                                "newText": { "type": "string", "description": "Replacement text" }
                            },
                            "required": ["oldText", "newText"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["path", "edits"],
                "additionalProperties": false
            }),
        }
    }

    /// The mode of the executor: calls run one at a time when it handles one at a time.
    fn execution_mode(&self) -> ExecutionMode {
        self.executor.execution_mode()
    }

    /// Shapes models commonly send instead of an `edits` array (pi: `prepareEditArguments`):
    /// the array as a JSON string, a single edit object, or `oldText` and `newText` at the top
    /// level, which become one more edit.
    fn prepare_arguments(&self, args: &Value) -> Option<Value> {
        let mut object = args.as_object()?.clone();
        let mut changed = false;
        if let Some(Value::String(text)) = object.get("edits") {
            match serde_json::from_str::<Value>(text) {
                Ok(parsed @ Value::Array(_)) => {
                    object.insert("edits".into(), parsed);
                    changed = true;
                }
                Ok(parsed) if is_single_edit(&parsed) => {
                    object.insert("edits".into(), Value::Array(vec![parsed]));
                    changed = true;
                }
                // Left as it is, so that the schema check explains what is wrong.
                Ok(_) | Err(_) => {}
            }
        } else if let Some(single) = object.get("edits").filter(|edits| is_single_edit(edits)).cloned() {
            object.insert("edits".into(), Value::Array(vec![single]));
            changed = true;
        }
        if let (Some(Value::String(_)), Some(Value::String(_))) = (object.get("oldText"), object.get("newText")) {
            let mut legacy = Map::new();
            for key in ["oldText", "newText"] {
                if let Some(value) = object.remove(key) {
                    legacy.insert(key.into(), value);
                }
            }
            match object.get_mut("edits") {
                Some(Value::Array(edits)) => edits.push(Value::Object(legacy)),
                _ => {
                    object.insert("edits".into(), Value::Array(vec![Value::Object(legacy)]));
                }
            }
            changed = true;
        }
        changed.then_some(Value::Object(object))
    }

    fn validate_arguments(&self, args: &Value) -> Result<(), String> {
        let args = Args::deserialize(args).map_err(|e| format!("invalid arguments: {e}"))?;
        if args.edits.is_empty() {
            return Err("edits must contain at least one replacement".into());
        }
        resolve(Path::new("."), &args.path).map(|_| ())
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, String> {
        let args: Args = serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))?;
        let shown = args.path.trim();
        let path = resolve(&ctx.cwd, &args.path)?;
        let cancelled = || "edit cancelled before the file was changed".to_string();
        let _turn = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return Err(cancelled()),
            turn = self.queue.turn(&path) => turn,
        };
        let bytes = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return Err(cancelled()),
            read = self.executor.read_file(&path, self.max_file_bytes) => read?,
        };
        if bytes.iter().take(8192).any(|&b| b == 0) {
            return Err(format!(
                "{} looks like a binary file; edit_file edits text only",
                path.display()
            ));
        }
        let raw = String::from_utf8(bytes).map_err(|e| {
            format!(
                "{} is not UTF-8 text ({e}); edit_file edits UTF-8 text only",
                path.display()
            )
        })?;
        let (bom, text) = split_bom(&raw);
        let ending = line_ending(text);
        let original = normalize_to_lf(text);
        let changed = apply_edits(&original, &args.edits, shown)?;
        let contents = format!("{bom}{}", restore_line_endings(&changed, ending));
        // Checked last before writing: a write is not interrupted once it has started.
        if ctx.cancel.is_cancelled() {
            return Err(cancelled());
        }
        self.executor.write_file(&path, contents.as_bytes()).await?;

        let mut details = Map::new();
        details.insert("path".into(), json!(path.display().to_string()));
        if let Some((patch, first_changed_line)) = unified_patch(shown, &original, &changed) {
            details.insert("patch".into(), json!(clip(&patch, MAX_PATCH_BYTES)));
            details.insert("firstChangedLine".into(), json!(first_changed_line));
        }
        let count = args.edits.len();
        let blocks = if count == 1 { "block" } else { "blocks" };
        Ok(ToolOutput::ok(format!("Replaced {count} {blocks} in {shown}.")).with_details(Value::Object(details)))
    }
}

/// Whether `value` is one edit object: `oldText` and `newText` strings.
fn is_single_edit(value: &Value) -> bool {
    value.get("oldText").is_some_and(Value::is_string) && value.get("newText").is_some_and(Value::is_string)
}

/// `text` cut at a character boundary to at most `max_bytes`, with a note when it was cut.
fn clip(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[patch cut at {end} of {} bytes]\n", &text[..end], text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared(args: Value) -> Option<Value> {
        EditFileTool::default().prepare_arguments(&args)
    }

    #[test]
    fn edits_sent_as_a_string_or_a_single_object_become_an_array() {
        let edit = json!({"oldText": "a", "newText": "b"});
        let expected = json!({"path": "f", "edits": [edit.clone()]});
        assert_eq!(
            prepared(json!({"path": "f", "edits": edit.to_string()})),
            Some(expected.clone())
        );
        assert_eq!(
            prepared(json!({"path": "f", "edits": edit.clone()})),
            Some(expected.clone())
        );
        assert_eq!(
            prepared(json!({"path": "f", "edits": json!([edit.clone()]).to_string()})),
            Some(expected)
        );
    }

    #[test]
    fn top_level_old_and_new_text_become_one_more_edit() {
        let first = json!({"oldText": "a", "newText": "b"});
        assert_eq!(
            prepared(json!({"path": "f", "oldText": "x", "newText": "y"})),
            Some(json!({"path": "f", "edits": [{"oldText": "x", "newText": "y"}]}))
        );
        assert_eq!(
            prepared(json!({"path": "f", "edits": [first.clone()], "oldText": "x", "newText": "y"})),
            Some(json!({"path": "f", "edits": [first, {"oldText": "x", "newText": "y"}]}))
        );
    }

    #[test]
    fn well_formed_and_unrecognized_arguments_are_left_alone() {
        assert_eq!(
            prepared(json!({"path": "f", "edits": [{"oldText": "a", "newText": "b"}]})),
            None
        );
        assert_eq!(prepared(json!({"path": "f", "edits": "not json"})), None);
        assert_eq!(prepared(json!({"path": "f", "oldText": 1, "newText": "y"})), None);
        assert_eq!(prepared(json!("text")), None);
    }

    #[test]
    fn a_long_patch_is_cut_at_a_character_boundary() {
        let text = "界".repeat(10);
        let clipped = clip(&text, 10);
        assert!(clipped.starts_with(&"界".repeat(3)), "{clipped}");
        assert!(clipped.contains("[patch cut at 9 of 30 bytes]"), "{clipped}");
        assert_eq!(clip("short", 10), "short");
    }
}
