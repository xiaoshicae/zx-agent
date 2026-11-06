//! The tool interface, and the set of tools a run declares.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::ai::ToolDef;
use crate::sync::lock;

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

    /// Queue a bounded progress update without blocking the tool. Text
    /// longer than [`MAX_PROGRESS_BYTES`] is cut at a character boundary. The loop passes
    /// updates through the redaction hooks, then emits them as `tool_execution_update`
    /// events; they are not recorded in the session and the model never sees them. Returns
    /// false when the queue ([`MAX_QUEUED_PROGRESS`] updates) is full, the call has finished,
    /// or this is a direct call without an observer; the update is then dropped.
    pub fn report_progress(&self, text: impl Into<String>) -> bool {
        let Some(progress) = &self.progress else { return false };
        let mut text = text.into();
        if text.len() > MAX_PROGRESS_BYTES {
            let mut end = MAX_PROGRESS_BYTES;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
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
pub const MAX_DETAILS_BYTES: usize = 16 * 1024;

/// What a tool returns. In JSON `content` is required and the flags default to `false`, so
/// output that a hook process forgot cannot silently replace the result with nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolOutput {
    /// Text returned to the model.
    pub content: String,
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

    /// The same result with structured `details` for observers.
    pub fn with_details(self, details: Value) -> Self {
        Self {
            details: Some(details),
            ..self
        }
    }
}

/// A tool the model can call.
///
/// Arguments arrive as raw JSON; tools validate them by deserializing into their own
/// argument type. `Err` becomes an error result that the model sees, and the loop
/// continues.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Name the model calls the tool by. Must match `definition().name`.
    fn name(&self) -> &str;
    /// Declaration sent to the model with every request.
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
    /// Rewrite the model's arguments before they are checked, for an older argument shape
    /// or a mistake models commonly make. Return `None` to keep
    /// them as they are, which the default does.
    ///
    /// The rewritten arguments are what the schema check, `validate_arguments`, the
    /// `before_tool` and `after_tool` hooks (the policy among them), approval and `execute`
    /// see; `tool_execution_start` still shows the model's. The session keeps the model's original call, so the
    /// rewrite must depend on nothing but `args`: an audit then derives the arguments that
    /// ran from the recorded ones. Direct SDK calls skip this step.
    fn prepare_arguments(&self, _args: &Value) -> Option<Value> {
        None
    }
    /// Validate before policy and approval, after the declared JSON Schema has been checked.
    /// Override for semantic checks;
    /// execute must still validate direct SDK calls. The default adds no checks.
    fn validate_arguments(&self, _args: &Value) -> Result<(), String> {
        Ok(())
    }
    /// Run the tool. Must honor `ctx.cancel` if it can take long, and bound its output.
    ///
    /// When the run is cancelled, the loop stops waiting at once and drops this future, so
    /// cleanup that must happen, such as ending a process, belongs in `Drop`; the built-in
    /// tools do so. `ctx.cancel` lets a tool stop early when it is called directly. The
    /// `name`, `definition`, `execution_mode` and `effects` methods must not panic: the loop
    /// tries them before a run's first request and fails the run if one does.
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, String>;
}

#[cfg(test)]
mod tests {
    use super::*;

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
