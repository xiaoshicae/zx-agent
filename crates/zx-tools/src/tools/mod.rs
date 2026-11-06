//! Built-in tools (pi: coding-agent/src/core/tools).

pub mod bash;
pub mod edit;
mod edit_text;
pub mod mutation;
pub mod read;
pub mod search;
pub mod truncate;
pub mod write;

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
pub use search::{FindTool, GrepTool, LsTool, SearchSettings};
pub use write::WriteFileTool;

use crate::agent::{Tool, ToolContext};
use crate::executor::{Executor, LocalExecutor};

/// Name of the built-in shell tool.
pub const BASH: &str = "bash";
/// Name of the built-in file reading tool.
pub const READ_FILE: &str = "read_file";
/// Name of the built-in tool that replaces text in a file.
pub const EDIT_FILE: &str = "edit_file";
/// Name of the built-in tool that creates or overwrites a file.
pub const WRITE_FILE: &str = "write_file";
/// Name of the built-in tool that searches file contents.
pub const GREP: &str = "grep";
/// Name of the built-in tool that finds files by glob.
pub const FIND: &str = "find";
/// Name of the built-in tool that lists a directory.
pub const LS: &str = "ls";

/// Names accepted by [`builtin_tools`].
pub const BUILTIN_TOOL_NAMES: &[&str] = &[BASH, READ_FILE, EDIT_FILE, WRITE_FILE, GREP, FIND, LS];

/// The largest file the text tools read or edit by default: enough for any source file, and
/// far more than one call can show.
const DEFAULT_MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// Build built-in tools by name, in the given order, without duplicates. `hidden_env` names
/// environment variables that bash commands must not see, such as the API key variable.
/// The tools run on this machine; see [`builtin_tools_with`] for another executor.
pub fn builtin_tools(names: &[String], bash_timeout: Duration, hidden_env: &[String]) -> Result<Vec<Arc<dyn Tool>>> {
    let local: Arc<dyn Executor> = Arc::new(LocalExecutor);
    builtin_tools_with(names, bash_timeout, hidden_env, &local)
}

/// Like [`builtin_tools`], with the tools running commands and reading and writing files through
/// `executor`. edit_file and write_file share one [`FileMutationQueue`].
pub fn builtin_tools_with(
    names: &[String],
    bash_timeout: Duration,
    hidden_env: &[String],
    executor: &Arc<dyn Executor>,
) -> Result<Vec<Arc<dyn Tool>>> {
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    let queue = Arc::new(FileMutationQueue::default());
    let search = SearchSettings {
        executor: Arc::clone(executor),
        hidden_env: hidden_env.to_vec(),
        ..SearchSettings::default()
    };
    for name in names {
        let name = name.trim();
        if name.is_empty() || tools.iter().any(|tool| tool.name() == name) {
            continue;
        }
        let tool: Arc<dyn Tool> = match name {
            BASH => Arc::new(BashTool {
                default_timeout: bash_timeout,
                max_timeout: bash_timeout.max(bash::MAX_TIMEOUT),
                hidden_env: hidden_env.to_vec(),
                executor: Arc::clone(executor),
                ..BashTool::default()
            }),
            READ_FILE => Arc::new(ReadFileTool {
                executor: Arc::clone(executor),
                ..ReadFileTool::default()
            }),
            EDIT_FILE => Arc::new(EditFileTool {
                executor: Arc::clone(executor),
                queue: queue.clone(),
                ..EditFileTool::default()
            }),
            WRITE_FILE => Arc::new(WriteFileTool {
                executor: Arc::clone(executor),
                queue: queue.clone(),
            }),
            GREP => Arc::new(GrepTool {
                settings: search.clone(),
            }),
            FIND => Arc::new(FindTool {
                settings: search.clone(),
            }),
            LS => Arc::new(LsTool {
                settings: search.clone(),
            }),
            other => bail!("unknown tool {other:?}; available: {}", BUILTIN_TOOL_NAMES.join(", ")),
        };
        tools.push(tool);
    }
    Ok(tools)
}

/// `args` as a tool's argument type; the error tells the model what was wrong.
pub(crate) fn parse_args<'de, T: Deserialize<'de>>(args: &'de Value) -> Result<T, String> {
    T::deserialize(args).map_err(|e| format!("invalid arguments: {e}"))
}

/// `raw` as a path: without a leading `@`, which some models put before paths, and against
/// `cwd` when relative.
pub(crate) fn resolve(cwd: &Path, raw: &str) -> Result<PathBuf, String> {
    let raw = raw.trim();
    let raw = raw.strip_prefix('@').unwrap_or(raw);
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

/// How far into a file the text tools look for a NUL byte, which marks a binary file.
const BINARY_SNIFF_BYTES: usize = 8192;

/// Whether `bytes` start like a binary file, which the text tools refuse.
pub(crate) fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_SNIFF_BYTES).any(|&b| b == 0)
}

/// `operation`, unless the call is cancelled first.
pub(crate) async fn unless_cancelled<T>(ctx: &ToolContext, operation: impl Future<Output = T>) -> Option<T> {
    tokio::select! {
        biased;
        () = ctx.cancel.cancelled() => None,
        value = operation => Some(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(tools: &[Arc<dyn Tool>]) -> Vec<&str> {
        tools.iter().map(|tool| tool.name()).collect()
    }

    #[test]
    fn builds_tools_in_order_without_duplicates_or_blanks() {
        let requested = ["read_file", " bash ", "read_file", ""].map(String::from);
        let tools = builtin_tools(&requested, Duration::from_secs(5), &[]).unwrap();
        assert_eq!(names(&tools), ["read_file", "bash"]);
    }

    #[test]
    fn rejects_unknown_tool_names() {
        let error = builtin_tools(&["curl".to_string()], Duration::from_secs(5), &[])
            .err()
            .unwrap()
            .to_string();
        assert!(error.starts_with("unknown tool \"curl\""), "{error}");
    }

    #[test]
    fn bash_gets_the_default_timeout_and_keeps_the_larger_maximum() {
        let tools = builtin_tools(&["bash".to_string()], Duration::from_secs(7), &[]).unwrap();
        let description = tools[0].definition().description;
        assert!(
            description.contains("after 7s unless timeout_secs is set (max 600s)"),
            "{description}"
        );
    }
}
