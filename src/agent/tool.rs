//! The tool interface.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::ai::ToolDef;

/// Whether a tool may run alongside other calls from the same assistant message.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExecutionMode {
    /// May run concurrently with other calls from the same assistant message.
    #[default]
    Parallel,
    /// Forces the whole batch of calls to run one at a time (pi: `executionMode: "sequential"`).
    Sequential,
}

/// What a tool receives besides its arguments.
#[derive(Clone, Debug)]
pub struct ToolContext {
    pub cwd: PathBuf,
    /// Cancelled when the run is aborted or times out. Long-running tools must honor it.
    pub cancel: CancellationToken,
}

/// What a tool returns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolOutput {
    /// Text returned to the model.
    pub content: String,
    pub is_error: bool,
    /// Ask to stop after this batch. Takes effect only when every result in the batch sets it.
    pub terminate: bool,
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
            terminate: false,
        }
    }
}

/// A tool the model can call.
///
/// Arguments arrive as raw JSON; tools validate them by deserializing into their own
/// argument type. `Err` is the equivalent of throwing in pi: it becomes an error result
/// that the model sees, and the loop continues.
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
    /// Validate before policy and approval. Override for schema and semantic checks;
    /// execute must still validate direct SDK calls. The default adds no checks.
    fn validate_arguments(&self, _args: &Value) -> Result<(), String> {
        Ok(())
    }
    /// Run the tool. Must honor `ctx.cancel` if it can take long, and bound its output.
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, String>;
}
