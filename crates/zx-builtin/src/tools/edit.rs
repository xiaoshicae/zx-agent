//! `edit_file` tool: exact text replacements in one file (pi: coding-agent/src/core/tools/edit.ts).

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use zx_core::tools::{ExecutionMode, Tool, ToolContext, ToolOutput};

use super::edit_text::{Edit, NO_EDITS, add_patch, apply_edits, split_bom};
use super::mutation::{CHANGE_CANCELLED, FileMutationQueue, write_locked};
use super::{DEFAULT_MAX_FILE_BYTES, check_path, looks_binary, parse_args, path_text, resolve, unless_cancelled};
use crate::ai::ToolDef;
use crate::executor::{Executor, LocalExecutor};

/// Replaces exact, unique, non-overlapping pieces of text in one file of at most
/// [`DEFAULT_MAX_FILE_BYTES`](super::DEFAULT_MAX_FILE_BYTES) bytes.
pub struct EditFileTool {
    /// Where files are read and written. Defaults to this machine.
    pub executor: Arc<dyn Executor>,
    /// Shared with `write_file`, so that changes to one file happen one at a time.
    pub queue: Arc<FileMutationQueue>,
}

impl Default for EditFileTool {
    fn default() -> Self {
        Self {
            executor: Arc::new(LocalExecutor::default()),
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

impl Args {
    /// The arguments of a call, checked.
    fn parse(args: &Value) -> Result<Self, String> {
        let args: Self = parse_args(args)?;
        if args.edits.is_empty() {
            return Err(NO_EDITS.into());
        }
        check_path(&args.path)?;
        Ok(args)
    }
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
            let mut top_level = Map::new();
            for key in ["oldText", "newText"] {
                if let Some(value) = object.remove(key) {
                    top_level.insert(key.into(), value);
                }
            }
            match object.get_mut("edits") {
                Some(Value::Array(edits)) => edits.push(Value::Object(top_level)),
                _ => {
                    object.insert("edits".into(), Value::Array(vec![Value::Object(top_level)]));
                }
            }
            changed = true;
        }
        changed.then_some(Value::Object(object))
    }

    fn validate_arguments(&self, args: &Value) -> Result<(), String> {
        Args::parse(args).map(|_| ())
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, String> {
        let args = Args::parse(&args)?;
        let shown = path_text(&args.path)?;
        let path = resolve(&ctx.cwd, &args.path)?;
        let turn = unless_cancelled(ctx, self.queue.turn(&path))
            .await
            .ok_or(CHANGE_CANCELLED)?;
        let read = unless_cancelled(ctx, self.executor.read_file(&path, DEFAULT_MAX_FILE_BYTES)).await;
        let bytes = read.ok_or(CHANGE_CANCELLED)??;
        if looks_binary(&bytes) {
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
        let changed = apply_edits(text, &args.edits, shown)?;
        let contents = format!("{bom}{changed}");
        write_locked(ctx, self.executor.clone(), path.clone(), contents.into_bytes(), turn).await?;

        let mut details = Map::new();
        details.insert("path".into(), json!(path.display().to_string()));
        add_patch(&mut details, shown, text, &changed);
        let count = args.edits.len();
        let blocks = if count == 1 { "block" } else { "blocks" };
        Ok(ToolOutput::ok(format!("Replaced {count} {blocks} in {shown}.")).with_details(Value::Object(details)))
    }
}

/// Whether `value` is one edit object: `oldText` and `newText` strings.
fn is_single_edit(value: &Value) -> bool {
    value.get("oldText").is_some_and(Value::is_string) && value.get("newText").is_some_and(Value::is_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared(args: &Value) -> Option<Value> {
        EditFileTool::default().prepare_arguments(args)
    }

    #[test]
    fn edits_sent_as_a_string_or_a_single_object_become_an_array() {
        let edit = json!({"oldText": "a", "newText": "b"});
        let expected = json!({"path": "f", "edits": [edit.clone()]});
        assert_eq!(
            prepared(&json!({"path": "f", "edits": edit.to_string()})),
            Some(expected.clone())
        );
        assert_eq!(
            prepared(&json!({"path": "f", "edits": edit.clone()})),
            Some(expected.clone())
        );
        assert_eq!(
            prepared(&json!({"path": "f", "edits": json!([edit.clone()]).to_string()})),
            Some(expected)
        );
    }

    #[test]
    fn top_level_old_and_new_text_become_one_more_edit() {
        let first = json!({"oldText": "a", "newText": "b"});
        assert_eq!(
            prepared(&json!({"path": "f", "oldText": "x", "newText": "y"})),
            Some(json!({"path": "f", "edits": [{"oldText": "x", "newText": "y"}]}))
        );
        assert_eq!(
            prepared(&json!({"path": "f", "edits": [first.clone()], "oldText": "x", "newText": "y"})),
            Some(json!({"path": "f", "edits": [first, {"oldText": "x", "newText": "y"}]}))
        );
    }

    #[test]
    fn well_formed_and_unrecognized_arguments_are_left_alone() {
        assert_eq!(
            prepared(&json!({"path": "f", "edits": [{"oldText": "a", "newText": "b"}]})),
            None
        );
        assert_eq!(prepared(&json!({"path": "f", "edits": "not json"})), None);
        assert_eq!(prepared(&json!({"path": "f", "oldText": 1, "newText": "y"})), None);
        assert_eq!(prepared(&json!("text")), None);
    }
}
