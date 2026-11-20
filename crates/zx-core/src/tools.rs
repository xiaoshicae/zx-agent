//! The tool interface, the tools of a session, and which of them a run declares.

use std::collections::HashSet;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::ai::{Image, ToolDef, Usage};
use crate::sync::lock;
use crate::text::prefix_within;

/// The tools of a session, found by the name the model calls them by. Each tool declares
/// itself once, when the toolbox is made: what requests declare, what arguments are checked
/// against, and how a call runs and is recorded is that one declaration.
#[derive(Default)]
pub(crate) struct Toolbox {
    /// In the order requests declare them.
    tools: Vec<Declared>,
}

/// A tool, and what it declared when the toolbox was made.
pub(crate) struct Declared {
    pub(crate) tool: Arc<dyn Tool>,
    definition: ToolDef,
    pub(crate) execution_mode: ExecutionMode,
    pub(crate) effects: Effects,
    /// The declared parameter schema, compiled.
    #[cfg(feature = "json-schema")]
    schema: jsonschema::Validator,
}

impl Toolbox {
    /// Check what a loop relies on: the model calls tools by the names they declare, so each
    /// tool must declare the name it is called by and no two may share one, and, with the
    /// `json-schema` feature, each declared parameter schema must be a JSON Schema. A tool's
    /// `name`, `definition`, `execution_mode` and `effects` must not panic; they are tried
    /// here, so that a panic refuses the tool instead of escaping a run.
    pub(crate) fn new(tools: Vec<Arc<dyn Tool>>) -> Result<Self, String> {
        let mut declared: Vec<Declared> = Vec::with_capacity(tools.len());
        for (index, tool) in tools.into_iter().enumerate() {
            let described = std::panic::catch_unwind(AssertUnwindSafe(|| {
                (
                    tool.name().to_string(),
                    tool.definition(),
                    tool.execution_mode(),
                    tool.effects(),
                )
            }));
            let Ok((name, definition, execution_mode, effects)) = described else {
                return Err(format!("tool {index} panicked while describing itself"));
            };
            if definition.name != name {
                return Err(format!(
                    "tool {name:?} declares itself as {:?}; the model calls a tool by the name it declares",
                    definition.name
                ));
            }
            if declared.iter().any(|other| other.definition.name == name) {
                return Err(format!("more than one tool is named {name:?}"));
            }
            #[cfg(feature = "json-schema")]
            let schema = jsonschema::validator_for(&definition.parameters)
                .map_err(|error| format!("tool {name:?} has an invalid parameter schema: {error}"))?;
            declared.push(Declared {
                tool,
                definition,
                execution_mode,
                effects,
                #[cfg(feature = "json-schema")]
                schema,
            });
        }
        Ok(Self { tools: declared })
    }

    /// The names the model calls the tools by, in the order requests declare them.
    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.iter().map(|declared| declared.definition.name.as_str())
    }

    /// The tool the model calls `name`, with how it runs beside other calls and what it
    /// changes, as it declared.
    pub(crate) fn declared(&self, name: &str) -> Option<&Declared> {
        self.tools.iter().find(|declared| declared.definition.name == name)
    }

    /// What the tool the model calls `name` changes, as it declared.
    pub(crate) fn effects(&self, name: &str) -> Option<Effects> {
        self.declared(name).map(|declared| declared.effects)
    }

    /// The declarations of the tools that `active` offers, for a request.
    pub(crate) fn definitions(&self, active: &ActiveTools) -> Vec<ToolDef> {
        self.tools
            .iter()
            .map(|declared| &declared.definition)
            .filter(|definition| active.is_active(&definition.name))
            .cloned()
            .collect()
    }

    /// Why `arguments` do not fit the parameter schema that the tool called `name` declares,
    /// when they do not.
    #[cfg(feature = "json-schema")]
    pub(crate) fn schema_mismatch(&self, name: &str, arguments: &Value) -> Option<String> {
        let schema = &self.declared(name)?.schema;
        schema.validate(arguments).err().map(|error| error.to_string())
    }

    /// Without the `json-schema` feature nothing is compared with the declared schema: each
    /// tool checks its own arguments.
    #[cfg(not(feature = "json-schema"))]
    #[expect(
        clippy::unused_self,
        reason = "the same signature as with the feature, so that callers need no cfg"
    )]
    pub(crate) fn schema_mismatch(&self, _name: &str, _arguments: &Value) -> Option<String> {
        None
    }
}

/// Which of the configured tools the model is offered (pi: `setActiveTools`). Shared by the
/// loop, the host and the hooks: a change applies to the next request, also in the middle
/// of a run, and the request audit records the tools each request declared. By default
/// every configured tool is active.
#[derive(Debug, Default)]
pub struct ActiveTools {
    /// `None` means every configured tool.
    names: Mutex<Option<HashSet<String>>>,
}

impl ActiveTools {
    /// Offer only `names` from the next request on. A name that no configured tool has
    /// selects nothing; the host checks names, hooks do not have to.
    pub fn set(&self, names: impl IntoIterator<Item = impl Into<String>>) {
        *lock(&self.names) = Some(names.into_iter().map(Into::into).collect());
    }

    /// Offer every configured tool again.
    pub fn reset(&self) {
        *lock(&self.names) = None;
    }

    /// The selected names, sorted; `None` while every configured tool is active.
    pub fn names(&self) -> Option<Vec<String>> {
        lock(&self.names).as_ref().map(|names| {
            let mut sorted: Vec<String> = names.iter().cloned().collect();
            sorted.sort();
            sorted
        })
    }

    /// Whether the tool called `name` is offered.
    pub fn is_active(&self, name: &str) -> bool {
        lock(&self.names).as_ref().is_none_or(|names| names.contains(name))
    }
}

/// Whether a tool may run alongside other calls from the same assistant message.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExecutionMode {
    /// May run concurrently with other calls from the same assistant message.
    #[default]
    Parallel,
    /// Forces the whole batch of calls to run one at a time.
    Sequential,
}

/// What a tool call can change. Recovery after an interrupted run depends on it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Effects {
    /// Only reads state, so running the call again is harmless.
    ReadOnly,
    /// May change files, processes or remote systems. The default: a tool that does not
    /// declare its effects is treated as one that changes state.
    #[default]
    SideEffects,
}

/// What a tool receives besides its arguments. Fields are only added over time, so code
/// outside this crate builds one with [`ToolContext::new`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ToolContext {
    pub cwd: PathBuf,
    /// Cancelled when the run is aborted or times out. Long-running tools must honor it. In
    /// a run it is the call's own token: a tool may cancel it to stop work of its own
    /// without stopping the run.
    pub cancel: CancellationToken,
    /// The model's tool call ID, absent for direct SDK calls.
    pub tool_call_id: Option<String>,
    /// Bounded progress channel, installed by the loop. Use `report_progress` to send.
    pub progress: Option<tokio::sync::mpsc::Sender<String>>,
}

impl ToolContext {
    /// Context for a direct SDK call, without a loop or progress events.
    pub fn new(cwd: PathBuf, cancel: CancellationToken) -> Self {
        Self {
            cwd,
            cancel,
            tool_call_id: None,
            progress: None,
        }
    }

    /// Queue a bounded progress update without blocking the tool. Text longer than
    /// [`MAX_PROGRESS_BYTES`] is cut at a character boundary. The loop passes updates
    /// through the redaction hooks, then emits them as `tool_execution_update` events; they
    /// are not recorded in the session and the model never sees them. Returns false when the
    /// queue ([`MAX_QUEUED_PROGRESS`] updates) is full, the call has finished, or this is a
    /// direct call without an observer; the update is then dropped.
    pub fn report_progress(&self, text: impl Into<String>) -> bool {
        let Some(progress) = &self.progress else { return false };
        let mut text = text.into();
        let kept = prefix_within(&text, MAX_PROGRESS_BYTES).len();
        text.truncate(kept);
        progress.try_send(text).is_ok()
    }
}

/// The most bytes of one progress update; longer text is cut.
pub const MAX_PROGRESS_BYTES: usize = 16 * 1024;

/// The most progress updates one call may have queued before the loop takes them: a burst
/// of a few lines between two polls of the loop fits, and a tool that keeps reporting faster
/// than the loop emits events holds at most `MAX_QUEUED_PROGRESS * MAX_PROGRESS_BYTES` of
/// updates in memory (512 KiB).
pub const MAX_QUEUED_PROGRESS: usize = 32;

/// The most bytes of [`ToolOutput::details`], as compact JSON. Details are recorded with
/// every result, so they get the same bound as a progress update.
pub const MAX_DETAILS_BYTES: usize = MAX_PROGRESS_BYTES;

/// What a tool returns. In JSON `content` is required and the flags default to `false`, so
/// output that a hook process forgot cannot silently replace the result with nothing.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolOutput {
    /// Text returned to the model.
    pub content: String,
    /// Images returned to the model after the text (pi: the image blocks of a result's
    /// `content`), fitted to the model APIs' limits (`zx_agent::builtin::images`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<Image>,
    #[serde(default)]
    pub is_error: bool,
    /// Ask to stop after this batch. Takes effect only when every result in the batch sets it.
    #[serde(default)]
    pub terminate: bool,
    /// Structured data for frontends and logs. After the `after_tool` hooks,
    /// the redaction hooks included, it is recorded with the result message and sent in the
    /// `tool_execution_end` event; the model never sees it. Details larger than
    /// [`MAX_DETAILS_BYTES`] as compact JSON are dropped from both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// What the tool spent on work of its own, such as a model it asked (pi: a result's
    /// `usage`). Recorded with the result, sent in the `tool_execution_end` event and summed
    /// by tool name in the run report; the model never sees it, and the token budget, which
    /// bounds the run's own model requests, leaves it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

impl ToolOutput {
    /// A successful result.
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            ..Default::default()
        }
    }

    /// A failed result. The model sees `content` as the error description.
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            ..Default::default()
        }
    }

    /// The same result with `images` for the model.
    pub fn with_images(self, images: Vec<Image>) -> Self {
        Self { images, ..self }
    }

    /// The same result with structured `details` for observers.
    pub fn with_details(self, details: Value) -> Self {
        Self {
            details: Some(details),
            ..self
        }
    }

    /// The same result with what the tool spent on work of its own.
    pub fn with_usage(self, usage: Usage) -> Self {
        Self {
            usage: Some(usage),
            ..self
        }
    }
}

/// A tool the model can call.
///
/// Arguments arrive as raw JSON; tools validate them by deserializing into their own
/// argument type. `Err` becomes an error result that the model sees, and the loop
/// continues.
///
/// `name`, `definition`, `execution_mode` and `effects` must not panic: they are tried when
/// an agent is made and when it makes a session, and a panic refuses the agent or the
/// session.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Name the model calls the tool by. Must match `definition().name`.
    fn name(&self) -> &str;

    /// Declaration sent to the model with every request, and whose parameter schema the
    /// arguments of a call are checked against. A session asks for it when it is made and
    /// keeps the answer.
    fn definition(&self) -> ToolDef;

    /// Defaults to `Parallel`.
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    /// Defaults to `SideEffects`. The session records that a call with side effects
    /// started before it runs, so recovery never assumes that an interrupted call did
    /// nothing. Return `ReadOnly` only when the tool cannot change any state.
    fn effects(&self) -> Effects {
        Effects::SideEffects
    }

    /// Rewrite the model's arguments before they are checked, to tolerate a mistake models
    /// commonly make, such as a JSON string where an array belongs or a field at the wrong
    /// level (pi: `prepareEditArguments`). Return `None` to keep them as they are, which the
    /// default does.
    ///
    /// The rewritten arguments are what the schema check, `validate_arguments`, the policies,
    /// approval, `execute` and the `after_tool` hooks see; `tool_execution_start` still shows
    /// the model's. The session keeps the model's original call, so the rewrite must depend on
    /// nothing but `args`: an audit then derives the arguments that ran from the recorded
    /// ones. Direct SDK calls skip this step.
    fn prepare_arguments(&self, _args: &Value) -> Option<Value> {
        None
    }

    /// Validate before policy and approval, after the declared JSON Schema has been checked.
    /// Override for semantic checks; `execute` must still validate direct SDK calls. The
    /// default adds no checks.
    fn validate_arguments(&self, _args: &Value) -> Result<(), String> {
        Ok(())
    }

    /// Run the tool. Must honor `ctx.cancel` if it can take long, and bound its output.
    ///
    /// When the run is cancelled, the loop stops waiting at once and drops this future, so
    /// cleanup that must happen, such as ending a process, belongs in `Drop`; the built-in
    /// tools do so. `ctx.cancel` lets a tool stop early when it is called directly.
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, String>;
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    use super::*;

    /// Declares another required argument each time it is asked: `v0`, then `v1`, and so on.
    struct Shifting(AtomicUsize);

    #[async_trait]
    impl Tool for Shifting {
        fn name(&self) -> &str {
            "shift"
        }

        fn definition(&self) -> ToolDef {
            let asked = self.0.fetch_add(1, Ordering::SeqCst);
            ToolDef {
                name: "shift".to_string(),
                description: "Shifts".to_string(),
                parameters: json!({"type": "object", "required": [format!("v{asked}")]}),
            }
        }

        async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, String> {
            Ok(ToolOutput::ok("shifted"))
        }
    }

    #[test]
    fn a_toolbox_declares_what_each_tool_declared_when_it_was_made() {
        let toolbox = Toolbox::new(vec![Arc::new(Shifting(AtomicUsize::new(0)))]).unwrap();
        let active = ActiveTools::default();

        let declared = toolbox.definitions(&active);

        assert_eq!(declared[0].parameters["required"], json!(["v0"]));
        assert_eq!(toolbox.definitions(&active), declared, "asked again, the same");
        assert_eq!(toolbox.names().collect::<Vec<_>>(), ["shift"]);
    }

    #[cfg(feature = "json-schema")]
    #[test]
    fn arguments_are_checked_against_the_schema_a_tool_declared_when_the_toolbox_was_made() {
        let toolbox = Toolbox::new(vec![Arc::new(Shifting(AtomicUsize::new(0)))]).unwrap();

        assert_eq!(toolbox.schema_mismatch("shift", &json!({"v0": 1})), None);
        assert!(toolbox.schema_mismatch("shift", &json!({"v1": 1})).is_some());
    }

    #[cfg(not(feature = "json-schema"))]
    #[test]
    fn without_the_schema_feature_arguments_are_left_to_the_tool() {
        let toolbox = Toolbox::new(vec![Arc::new(Shifting(AtomicUsize::new(0)))]).unwrap();

        assert_eq!(toolbox.schema_mismatch("shift", &json!({"v1": 1})), None);
    }

    #[test]
    fn a_toolbox_offers_only_the_active_tools_and_finds_every_tool() {
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(Shifting(AtomicUsize::new(0)))];
        let toolbox = Toolbox::new(tools).unwrap();
        let active = ActiveTools::default();
        active.set(["another"]);

        assert!(toolbox.definitions(&active).is_empty());
        assert!(
            toolbox.declared("shift").is_some(),
            "an inactive tool is still the session's"
        );
        assert!(toolbox.declared("another").is_none());
    }

    fn observed(capacity: usize) -> (ToolContext, tokio::sync::mpsc::Receiver<String>) {
        let (progress, updates) = tokio::sync::mpsc::channel(capacity);
        let ctx = ToolContext {
            progress: Some(progress),
            ..ToolContext::new(PathBuf::from("."), CancellationToken::new())
        };
        (ctx, updates)
    }

    #[test]
    fn a_direct_call_has_nobody_to_report_progress_to() {
        let ctx = ToolContext::new(PathBuf::from("."), CancellationToken::new());
        assert!(!ctx.report_progress("step"));
    }

    #[test]
    fn long_progress_is_cut_at_a_character_boundary() {
        let (ctx, mut updates) = observed(1);
        // Three bytes each, so the limit falls inside a character.
        assert!(ctx.report_progress("界".repeat(MAX_PROGRESS_BYTES)));
        let update = updates.try_recv().unwrap();
        assert_eq!(update.len(), MAX_PROGRESS_BYTES / 3 * 3);
        assert!(update.chars().all(|c| c == '界'));
    }

    #[test]
    fn progress_is_dropped_when_the_queue_is_full() {
        let (ctx, mut updates) = observed(1);
        assert!(ctx.report_progress("first"));
        assert!(!ctx.report_progress("second"));
        assert_eq!(updates.try_recv().unwrap(), "first");
        assert!(updates.try_recv().is_err());
    }
}
