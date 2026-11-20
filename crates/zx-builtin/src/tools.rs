//! Built-in tools (pi: coding-agent/src/core/tools). Each reads and checks its arguments in
//! one `Args::parse`: before policy and approval, in `validate_arguments`, and again in
//! `execute`, which a host may call directly.
//!
//! - `bash`: run a shell command
//! - `read`, `write` and `edit`: read a file in pages, write a whole file, replace exact text;
//!   `edit_text` holds the replacement and its patch, `mutation` the queue that changes one file
//!   at a time
//! - `search`: `grep`, `find` and `ls`
//! - `truncate`: the bounds every tool's output keeps
//! - `paging`: text a tool returns a page at a time, and the files of this machine read for it

mod bash;
mod edit;
mod edit_text;
mod mutation;
pub mod paging;
mod read;
mod search;
pub mod truncate;
mod write;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::Value;

pub use bash::BashTool;
pub use edit::EditFileTool;
pub use mutation::FileMutationQueue;
pub use read::ReadFileTool;
pub use search::{FindTool, GrepTool, LsTool};
pub use write::WriteFileTool;

use crate::executor::Executor;
use zx_core::tools::{Tool, ToolContext};

/// Name of the built-in shell tool.
pub const BASH: &str = "bash";
/// Name of the built-in file reading tool.
pub const READ_FILE: &str = "read_file";
/// Name of the built-in tool that replaces text in a file.
pub const EDIT_FILE: &str = "edit_file";
/// The key of [`EDIT_FILE`]'s result details that holds the unified patch of its change,
/// for a front end that shows what changed.
pub const PATCH_DETAIL: &str = "patch";
/// Name of the built-in tool that creates or overwrites a file.
pub const WRITE_FILE: &str = "write_file";
/// Name of the built-in tool that searches file contents.
pub const GREP: &str = "grep";
/// Name of the built-in tool that finds files by glob.
pub const FIND: &str = "find";
/// Name of the built-in tool that lists a directory.
pub const LS: &str = "ls";
/// Name of the built-in tool that reads on in a stored tool result:
/// [`ReadOutputTool`](crate::output_store::ReadOutputTool), which comes with the hook that
/// stores long results.
pub const READ_OUTPUT: &str = "read_output";

/// The tools that run commands or read and write files through an executor: the names
/// [`executor_tools`] builds.
pub const EXECUTOR_TOOL_NAMES: &[&str] = &[BASH, READ_FILE, EDIT_FILE, WRITE_FILE, GREP, FIND, LS];

/// The largest file edit_file edits and, by default, read_file reads: enough for any source
/// file, and far more than one call can show.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// Build the tools of [`EXECUTOR_TOOL_NAMES`] named in `names`, in the given order, without
/// duplicates, running commands and reading and writing files through `executor`, which also
/// decides what commands must not see (see [`LocalExecutor::hidden_env`]). edit_file and
/// write_file wait for their turn at each file in `queue`, which the tools of other sessions
/// may share to coordinate with them. Errors: a name that is not in [`EXECUTOR_TOOL_NAMES`].
///
/// [`LocalExecutor::hidden_env`]: crate::executor::LocalExecutor::hidden_env
pub fn executor_tools(
    names: &[String],
    bash_timeout: Duration,
    executor: &Arc<dyn Executor>,
    queue: &Arc<FileMutationQueue>,
) -> Result<Vec<Arc<dyn Tool>>> {
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    for name in names {
        let name = name.trim();
        if name.is_empty() || tools.iter().any(|tool| tool.name() == name) {
            continue;
        }
        let tool: Arc<dyn Tool> = match name {
            BASH => Arc::new(BashTool {
                default_timeout: bash_timeout,
                max_timeout: bash_timeout.max(bash::MAX_TIMEOUT),
                executor: Arc::clone(executor),
            }),
            READ_FILE => Arc::new(ReadFileTool {
                executor: Arc::clone(executor),
                ..ReadFileTool::default()
            }),
            EDIT_FILE => Arc::new(EditFileTool {
                executor: Arc::clone(executor),
                queue: Arc::clone(queue),
            }),
            WRITE_FILE => Arc::new(WriteFileTool {
                executor: Arc::clone(executor),
                queue: Arc::clone(queue),
            }),
            GREP => Arc::new(GrepTool {
                executor: Arc::clone(executor),
            }),
            FIND => Arc::new(FindTool {
                executor: Arc::clone(executor),
            }),
            LS => Arc::new(LsTool {
                executor: Arc::clone(executor),
            }),
            other => bail!("unknown tool {other:?}; available: {}", EXECUTOR_TOOL_NAMES.join(", ")),
        };
        tools.push(tool);
    }
    Ok(tools)
}

/// `args` as a tool's argument type; the error tells the model what was wrong. Public, not
/// part of the API: zx-app's read_skill uses it.
#[doc(hidden)]
pub fn parse_args<'de, T: Deserialize<'de>>(args: &'de Value) -> Result<T, String> {
    T::deserialize(args).map_err(|e| format!("invalid arguments: {e}"))
}

/// `raw` as a path, read with [`path_text`], and against `cwd` when relative.
pub(crate) fn resolve(cwd: &Path, raw: &str) -> Result<PathBuf, String> {
    let raw = path_text(raw);
    if raw.is_empty() {
        return Err("path is empty".to_string());
    }
    let path = Path::new(raw);
    Ok(if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    })
}

/// The path a model wrote as `raw`: trimmed, and without a leading `@`, which some models put
/// before paths.
fn path_text(raw: &str) -> &str {
    let raw = raw.trim();
    raw.strip_prefix('@').unwrap_or(raw)
}

/// Check that `raw` names a path, before the call runs: the working directory is not known
/// yet, so only the form is checked, as [`resolve`] reads it.
pub(crate) fn check_path(raw: &str) -> Result<(), String> {
    resolve(Path::new("."), raw).map(|_| ())
}

/// How far into a file the text tools look for a NUL byte, which marks a binary file.
const BINARY_SNIFF_BYTES: usize = 8192;

/// Whether `bytes` start like a binary file, which the text tools refuse.
pub(crate) fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_SNIFF_BYTES).any(|&b| b == 0)
}

/// The error of a read that the call's cancellation stopped. Public, not part of the API:
/// zx-app's read_skill uses it.
#[doc(hidden)]
pub const READ_CANCELLED: &str = "the read was cancelled";

/// `operation`, unless the call is cancelled first. Public, not part of the API: zx-app's
/// read_skill uses it.
#[doc(hidden)]
pub async fn unless_cancelled<T>(ctx: &ToolContext, operation: impl Future<Output = T>) -> Option<T> {
    tokio::select! {
        biased;
        () = ctx.cancel.cancelled() => None,
        value = operation => Some(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::LocalExecutor;

    fn local() -> Arc<dyn Executor> {
        Arc::new(LocalExecutor::default())
    }

    fn names(tools: &[Arc<dyn Tool>]) -> Vec<&str> {
        tools.iter().map(|tool| tool.name()).collect()
    }

    #[test]
    fn builds_tools_in_order_without_duplicates_or_blanks() {
        let requested = ["read_file", " bash ", "read_file", ""].map(String::from);
        let tools = executor_tools(&requested, Duration::from_secs(5), &local(), &Arc::default()).unwrap();
        assert_eq!(names(&tools), ["read_file", "bash"]);
    }

    #[test]
    fn every_executor_tool_name_builds_its_tool() {
        let requested: Vec<String> = EXECUTOR_TOOL_NAMES.iter().map(ToString::to_string).collect();
        let tools = executor_tools(&requested, Duration::from_secs(5), &local(), &Arc::default()).unwrap();
        assert_eq!(names(&tools), EXECUTOR_TOOL_NAMES);
    }

    #[test]
    fn rejects_unknown_tool_names() {
        // read_output reads the results a hook stored, not through an executor.
        for name in ["curl", READ_OUTPUT] {
            let error = executor_tools(&[name.to_string()], Duration::from_secs(5), &local(), &Arc::default())
                .err()
                .unwrap()
                .to_string();
            assert!(error.starts_with(&format!("unknown tool {name:?}")), "{error}");
        }
    }

    #[test]
    fn bash_gets_the_default_timeout_and_keeps_the_larger_maximum() {
        let tools = executor_tools(&["bash".to_string()], Duration::from_secs(7), &local(), &Arc::default()).unwrap();
        let description = tools[0].definition().description;
        assert!(
            description.contains("after 7s unless timeoutSecs is set (max 600s)"),
            "{description}"
        );
    }
}
